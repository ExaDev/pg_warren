//! `warren.progress()` (DESIGN.md §5.7, §14 item 49): a row per backend
//! running a command, with its command frame and the operation inside it;
//! an operation with no command around it is its own command. A graph is
//! shown by its name, as the caller's snapshot sees it in this database,
//! or else by its id.
//!
//! Privacy follows `pg_stat_activity` and `pg_stat_progress_*`
//! (`visibility`): a row's pid, database, role, command and start times
//! are everyone's, the rest only for callers with the privileges of its
//! role or of `pg_read_all_stats`. Without the table (PG 15 and 16 without
//! the preload) only the caller's own row shows, with a `NOTICE`.

use std::collections::HashMap;

use pgrx::pg_sys::Oid;
use pgrx::prelude::*;
use pgrx::JsonB;

use super::backend::{claim, located};
use super::backends::{notice_without_table, own_holding};
use super::frame::{Frame, Operation};
use super::slot::{Holder, Values};
use super::visibility::shown_to_caller;
use crate::access::{as_catalog_owner, entry, AccessError};

/// One row of `warren.progress()`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    pub(crate) holder: Holder,
    /// The command, or the operation as its own.
    pub(crate) command: Frame,
    pub(crate) operation: Operation,
    /// Whether the caller sees more than who runs what since when.
    pub(crate) shown: bool,
}

/// The rows of `backends`, by pid, for those running a command, showing
/// details where `shown` says so for a backend's role.
pub(crate) fn rows<'v>(
    backends: impl IntoIterator<Item = &'v Values>,
    shown: impl Fn(Oid) -> bool,
) -> Vec<Row> {
    let mut rows: Vec<Row> = backends
        .into_iter()
        .filter(|values| values.command.is_some() || values.operation.frame.is_some())
        .map(|values| Row {
            holder: values.holder,
            command: match values.command.is_some() {
                true => values.command,
                false => values.operation.frame,
            },
            operation: values.operation,
            shown: shown(values.holder.role),
        })
        .collect();
    rows.sort_by_key(|row| row.holder.pid);
    rows
}

/// The rows now: from the table, or, without one, the caller's own.
fn current_rows() -> Vec<Row> {
    claim();
    match located() {
        Ok((table, _)) => {
            let copied = table.table().copy();
            rows(copied.backends.iter().map(|(_, values)| values), shown_to_caller)
        }
        Err(_) => {
            notice_without_table();
            rows([&own_holding()], shown_to_caller)
        }
    }
}

/// The names of the graphs of `rows` in this database that the caller
/// sees, by id, as the statement's snapshot sees them.
fn graph_names(rows: &[Row]) -> Result<HashMap<i32, String>, AccessError> {
    // SAFETY: the backend's database, set before any query.
    let here = unsafe { pgrx::pg_sys::MyDatabaseId };
    let mut ids: Vec<i32> = rows
        .iter()
        .filter(|row| row.shown && row.holder.database == here)
        .filter_map(|row| row.command.graph)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    let mut names = HashMap::new();
    if ids.is_empty() {
        return Ok(names);
    }
    as_catalog_owner(|| {
        Spi::connect(|client| {
            let query = "SELECT graph_id, name FROM warren.graphs WHERE graph_id = ANY ($1)";
            for row in client.select(query, None, &[ids.into()])? {
                if let (Some(id), Some(name)) = (row.get::<i32>(1)?, row.get::<String>(2)?) {
                    names.insert(id, name);
                }
            }
            Ok::<_, AccessError>(())
        })
    })?;
    Ok(names)
}

/// `value` where the caller sees a row's details.
fn shown<T>(shown: bool, value: Option<T>) -> Option<T> {
    value.filter(|_| shown)
}

/// A count as `bigint`.
fn count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// An operation's counters as `jsonb`.
fn details(operation: &Operation) -> JsonB {
    let map = operation
        .details()
        .map(|(name, value)| (name.to_owned(), serde_json::json!(value)))
        .collect::<serde_json::Map<_, _>>();
    JsonB(serde_json::Value::Object(map))
}

/// A frame's start, if it is a frame.
fn start(frame: &Frame) -> Option<TimestampWithTimeZone> {
    frame.is_some().then(|| TimestampWithTimeZone::try_from(frame.start).ok()).flatten()
}

/// A frame's text, if it is a frame.
fn text(frame: &Frame, field: impl Fn(&Frame) -> &str) -> Option<String> {
    frame.is_some().then(|| field(frame).to_owned())
}

/// `warren.progress()`: see the module's documentation. `PARALLEL SAFE`:
/// it reads no graph.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."progress"() RETURNS TABLE (
	"pid" int,
	"database" oid,
	"role" regrole,
	"command" text,
	"graph" text,
	"version" bigint,
	"phase" text,
	"done" bigint,
	"total" bigint,
	"operation" text,
	"operation_phase" text,
	"operation_done" bigint,
	"operation_total" bigint,
	"detail" jsonb,
	"command_start" timestamptz,
	"operation_start" timestamptz
)
VOLATILE PARALLEL SAFE
LANGUAGE c AS 'MODULE_PATHNAME', 'progress_wrapper';
"#
)]
// pgrx reads the columns' names from the return type itself, so they
// cannot be factored into a type alias.
#[allow(clippy::type_complexity)]
fn progress() -> TableIterator<
    'static,
    (
        name!(pid, i32),
        name!(database, Oid),
        name!(role, Oid),
        name!(command, String),
        name!(graph, Option<String>),
        name!(version, Option<i64>),
        name!(phase, Option<String>),
        name!(done, Option<i64>),
        name!(total, Option<i64>),
        name!(operation, Option<String>),
        name!(operation_phase, Option<String>),
        name!(operation_done, Option<i64>),
        name!(operation_total, Option<i64>),
        name!(detail, Option<JsonB>),
        name!(command_start, Option<TimestampWithTimeZone>),
        name!(operation_start, Option<TimestampWithTimeZone>),
    ),
> {
    entry(|_| {
        let rows = current_rows();
        let names = graph_names(&rows)?;
        let rows = rows.into_iter().map(|row| {
            let (command, operation) = (row.command, row.operation.frame);
            let graph = command.graph.map(|id| names.get(&id).cloned().unwrap_or(id.to_string()));
            (
                row.holder.pid,
                row.holder.database,
                row.holder.role,
                command.name.as_str().to_owned(),
                shown(row.shown, graph),
                shown(row.shown, command.version),
                shown(row.shown, Some(command.phase.as_str().to_owned())),
                shown(row.shown, command.done.map(count)),
                shown(row.shown, command.total.map(count)),
                text(&operation, |frame| frame.name.as_str()),
                shown(row.shown, text(&operation, |frame| frame.phase.as_str())),
                shown(row.shown, operation.done.map(count)),
                shown(row.shown, operation.total.map(count)),
                shown(row.shown, operation.is_some().then(|| details(&row.operation))),
                start(&command),
                start(&operation),
            )
        });
        Ok::<_, AccessError>(TableIterator::new(rows.collect::<Vec<_>>()))
    })
}

#[cfg(test)]
mod listing {
    use super::*;
    use crate::activity::frame::Name;

    const ALICE: Oid = Oid::from_u32(20_001);
    const BOB: Oid = Oid::from_u32(20_002);

    fn running(pid: i32, role: Oid, command: Option<&str>, operation: Option<&str>) -> Values {
        let frame = |name: &str| Frame { name: Name::new(name), start: 5, ..Frame::NONE };
        let mut values = Values {
            holder: Holder { pid, database: Oid::from(1), role, started: 1 },
            ..Values::FREE
        };
        values.command = command.map_or(Frame::NONE, frame);
        values.operation =
            Operation { frame: operation.map_or(Frame::NONE, frame), ..values.operation };
        values
    }

    #[test]
    fn rows_list_the_backends_running_a_command_by_pid() {
        let backends = [
            running(30, BOB, Some("louvain"), Some("local_moving")),
            running(20, ALICE, None, None),
            running(10, ALICE, None, Some("refresh")),
        ];
        let rows = rows(&backends, |role| role == ALICE);
        let listed: Vec<_> = rows
            .iter()
            .map(|row| {
                let operation = row.operation.frame.name;
                (
                    row.holder.pid,
                    row.command.name.as_str().to_owned(),
                    operation.as_str().to_owned(),
                    row.shown,
                )
            })
            .collect();
        assert_eq!(
            listed,
            [
                (10, "refresh".to_owned(), "refresh".to_owned(), true),
                (30, "louvain".to_owned(), "local_moving".to_owned(), false),
            ],
            "an operation without a command is its own command; bob's details are hidden"
        );
    }
}
