//! `warren.backends()` (DESIGN.md §5.7, §14 item 49): a row per backend that
//! has used pg_warren, with what its private cache holds, the shared bytes
//! it maps, its cache counters and the compute threads its running call
//! holds, then a `cluster` row of every backend's counters, those of
//! backends that have exited included, and the compute threads every
//! backend's calls hold, the count `warren.cluster_threads_limit` bounds
//! (§14 item 52). There is no reset: monitoring takes differences, as with
//! `pg_stat_*` counters.
//!
//! Privacy follows `pg_stat_activity` (`visibility`): a row's pid, database,
//! role and start are everyone's, the rest only for callers with the
//! privileges of its role or of `pg_read_all_stats`. The `cluster` row's
//! sizes and counts are everyone's. On PG 15 and 16 without the preload
//! there is no table: only the caller's own row, from its own copy, with a
//! `NOTICE`.

use std::sync::atomic::Ordering;

use pgrx::pg_sys::Oid;
use pgrx::prelude::*;
use pgrx::JsonB;

use super::backend::{between_tries, claim, located, own};
use super::slot::{Counters, Holder, Memory, Refused, Values};
use super::table::Copied;
use super::visibility::shown_to_caller;
use crate::access::{entry, AccessError};

/// One row of `warren.backends()`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Row {
    /// `backend`, or `cluster` for the totals.
    pub(crate) kind: &'static str,
    /// The backend's, none on the `cluster` row.
    pub(crate) holder: Option<Holder>,
    /// What it holds and counts; none where the caller may not see them.
    pub(crate) details: Option<Details>,
    /// When the backend started; for the `cluster` row, when the table was
    /// created.
    pub(crate) since: i64,
}

/// What a row holds and counts.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Details {
    pub(crate) memory: Memory,
    pub(crate) counters: Counters,
    /// The compute threads held: by the backend's running call, or, on the
    /// `cluster` row, by every backend's calls.
    pub(crate) compute_threads: u32,
}

/// The rows of `copied` (the table, created at `created`, whose header
/// counts `compute_threads` held), showing details where `shown` says so
/// for a backend's role: a row per backend, by pid, then the `cluster` row.
pub(crate) fn rows(
    copied: &Copied,
    created: i64,
    compute_threads: u32,
    shown: impl Fn(Oid) -> bool,
) -> Vec<Row> {
    let mut backends: Vec<&Values> = copied.backends.iter().map(|(_, values)| values).collect();
    backends.sort_by_key(|values| values.holder.pid);
    let mut rows: Vec<Row> = backends.iter().map(|values| backend_row(values, &shown)).collect();
    let memory = backends.iter().fold(Memory::default(), |sum, values| Memory {
        versions: sum.versions.saturating_add(values.memory.versions),
        vectors: sum.vectors.saturating_add(values.memory.vectors),
        transients: sum.transients.saturating_add(values.memory.transients),
        entries: sum.entries.saturating_add(values.memory.entries),
        mapped: sum.mapped.saturating_add(values.memory.mapped),
    });
    rows.push(Row {
        kind: "cluster",
        holder: None,
        details: Some(Details { memory, counters: copied.totals(), compute_threads }),
        since: created,
    });
    rows
}

/// A backend's row.
fn backend_row(values: &Values, shown: &impl Fn(Oid) -> bool) -> Row {
    Row {
        kind: "backend",
        holder: Some(values.holder),
        details: shown(values.holder.role).then_some(Details {
            memory: values.memory,
            counters: values.counters,
            compute_threads: values.compute_threads,
        }),
        since: values.holder.started,
    }
}

/// The rows now: from the table, or, without one, the caller's own.
fn current_rows() -> Vec<Row> {
    claim();
    match located() {
        Ok(table) => {
            let copied = table.table().copy(&mut between_tries());
            let held = table.compute_threads().load(Ordering::Acquire);
            rows(&copied, table.created(), held, shown_to_caller)
        }
        Err(_) => {
            notice_without_table();
            vec![backend_row(&own_holding(), &shown_to_caller)]
        }
    }
}

/// This backend's values, with its identity even if it has no slot.
pub(super) fn own_holding() -> Values {
    let values = own();
    match values.holder.pid {
        0 => Values { holder: super::backend::holder(), ..values },
        _ => values,
    }
}

/// The `NOTICE` that, without the table, only the caller's own row shows.
pub(super) fn notice_without_table() {
    ereport!(
        NOTICE,
        PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION,
        "other backends' rows need the activity table, which on PostgreSQL 15 and 16 needs \
         shared_preload_libraries = 'pg_warren': only this backend's row is shown"
    );
}

/// `refusals` as `jsonb`: a count per reason.
fn refusals(counters: &Counters) -> JsonB {
    let map = Refused::ALL
        .iter()
        .map(|reason| (reason.name().to_owned(), counters.refused(*reason).into()))
        .collect::<serde_json::Map<_, _>>();
    JsonB(serde_json::Value::Object(map))
}

/// A count as `bigint`.
fn count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `warren.backends()`: see the module's documentation. `PARALLEL SAFE`:
/// it reads no graph.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."backends"() RETURNS TABLE (
	"kind" text,
	"pid" int,
	"database" oid,
	"role" regrole,
	"private_bytes" bigint,
	"transient_bytes" bigint,
	"mapped_bytes" bigint,
	"lookups" bigint,
	"private_hits" bigint,
	"shared_hits" bigint,
	"waits" bigint,
	"private_loads" bigint,
	"shared_loads" bigint,
	"loaded_bytes" bigint,
	"load_seconds" float8,
	"private_evictions" bigint,
	"shared_evictions" bigint,
	"refusals" jsonb,
	"since" timestamptz,
	"compute_threads" bigint
)
VOLATILE PARALLEL SAFE
LANGUAGE c AS 'MODULE_PATHNAME', 'backends_wrapper';
"#
)]
// pgrx reads the columns' names from the return type itself, so they
// cannot be factored into a type alias.
#[allow(clippy::type_complexity)]
fn backends() -> TableIterator<
    'static,
    (
        name!(kind, String),
        name!(pid, Option<i32>),
        name!(database, Option<Oid>),
        name!(role, Option<Oid>),
        name!(private_bytes, Option<i64>),
        name!(transient_bytes, Option<i64>),
        name!(mapped_bytes, Option<i64>),
        name!(lookups, Option<i64>),
        name!(private_hits, Option<i64>),
        name!(shared_hits, Option<i64>),
        name!(waits, Option<i64>),
        name!(private_loads, Option<i64>),
        name!(shared_loads, Option<i64>),
        name!(loaded_bytes, Option<i64>),
        name!(load_seconds, Option<f64>),
        name!(private_evictions, Option<i64>),
        name!(shared_evictions, Option<i64>),
        name!(refusals, Option<JsonB>),
        name!(since, Option<TimestampWithTimeZone>),
        name!(compute_threads, Option<i64>),
    ),
> {
    entry(|_| {
        let rows = current_rows().into_iter().map(|row| {
            let detail = row.details;
            let memory = |f: fn(&Memory) -> u64| detail.map(|d| count(f(&d.memory)));
            let counter = |f: fn(&Counters) -> u64| detail.map(|d| count(f(&d.counters)));
            (
                row.kind.to_owned(),
                row.holder.map(|holder| holder.pid),
                row.holder.map(|holder| holder.database),
                row.holder.map(|holder| holder.role),
                memory(|m| m.versions.saturating_add(m.vectors)),
                memory(|m| m.transients),
                memory(|m| m.mapped),
                counter(|c| c.lookups),
                counter(|c| c.private_hits),
                counter(|c| c.shared_hits),
                counter(|c| c.waits),
                counter(|c| c.private_loads),
                counter(|c| c.shared_loads),
                counter(|c| c.loaded_bytes),
                detail.map(|d| d.counters.load_microseconds as f64 / 1e6),
                counter(|c| c.private_evictions),
                counter(|c| c.shared_evictions),
                detail.map(|d| refusals(&d.counters)),
                TimestampWithTimeZone::try_from(row.since).ok(),
                detail.map(|d| i64::from(d.compute_threads)),
            )
        });
        Ok::<_, AccessError>(TableIterator::new(rows.collect::<Vec<_>>()))
    })
}

#[cfg(test)]
mod listing {
    use super::*;

    const ALICE: Oid = Oid::from_u32(20_001);
    const BOB: Oid = Oid::from_u32(20_002);

    fn backend(pid: i32, role: Oid, lookups: u64, versions: u64) -> (usize, Values) {
        let holder = Holder { pid, database: Oid::from(5), role, started: 100 + i64::from(pid) };
        let counters = Counters { lookups, ..Counters::default() };
        let memory = Memory { versions, ..Memory::default() };
        let compute_threads = pid as u32 / 10;
        (pid as usize, Values { holder, memory, compute_threads, counters, ..Values::FREE })
    }

    #[test]
    fn rows_list_each_backend_by_pid_then_the_clusters_totals() {
        let exited = Counters { lookups: 7, ..Counters::default() };
        let copied =
            Copied { exited, backends: vec![backend(30, BOB, 2, 20), backend(10, ALICE, 1, 10)] };
        let rows = rows(&copied, 42, 5, |role| role == ALICE);
        let pids: Vec<_> = rows.iter().map(|row| row.holder.map(|holder| holder.pid)).collect();
        assert_eq!(pids, [Some(10), Some(30), None]);
        assert_eq!(rows[0].details.map(|d| d.counters.lookups), Some(1), "alice's");
        assert_eq!(rows[0].details.map(|d| d.compute_threads), Some(1), "the threads it holds");
        assert_eq!(rows[1].details, None, "bob's details are hidden from alice");
        assert_eq!(rows[1].holder.map(|holder| holder.role), Some(BOB), "but not who it is");
        assert_eq!(rows[1].since, 130);
        let cluster = &rows[2];
        assert_eq!(cluster.kind, "cluster");
        let Details { memory, counters, compute_threads } = cluster.details.unwrap();
        assert_eq!(counters.lookups, 10, "everyone's, the exited included");
        assert_eq!(memory.versions, 30, "the live backends' memory");
        assert_eq!(compute_threads, 5, "the threads the table's header counts held");
        assert_eq!(cluster.since, 42, "since the table was created");
    }

    #[test]
    fn refusals_name_every_reason() {
        let mut counters = Counters::default();
        counters.refuse(Refused::NoRoom);
        let JsonB(value) = refusals(&counters);
        assert_eq!(value["no_room"], 1);
        assert_eq!(value["unavailable"], 0);
        assert_eq!(value.as_object().unwrap().len(), Refused::ALL.len());
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::cache::end_statement;
    use crate::fixture::{refresh, ring};

    /// This backend's counters in `warren.backends()`, as
    /// `lookups private_hits private_loads unavailable`.
    fn counters() -> Result<Option<String>, Error> {
        Spi::get_one::<String>(
            "SELECT format('%s %s %s %s', lookups, private_hits, private_loads,
                           refusals ->> 'unavailable')
             FROM warren.backends() WHERE kind = 'backend' AND pid = pg_backend_pid()",
        )
    }

    /// The difference between two `counters()`.
    fn delta(before: &str, after: &str) -> Vec<i64> {
        let parse = |text: &str| -> Vec<i64> {
            text.split(' ').map(|count| count.parse().unwrap_or(-1)).collect()
        };
        parse(after).iter().zip(parse(before)).map(|(a, b)| a - b).collect()
    }

    /// Counters count once per statement and version, as the pin is taken
    /// (DESIGN.md §5.7): a load, then a hit in the next statement; the
    /// shared cache is off in these tests (`warren.shared_cache_memory` is
    /// 0), so each miss is refused as unavailable first.
    #[pg_test]
    fn a_backend_counts_each_version_once_per_statement() -> Result<(), Error> {
        ring("counted", 50, 1)?;
        refresh("counted", true)?;
        end_statement();
        let before = counters()?.ok_or(Error::InvalidPosition)?;
        Spi::run("SELECT warren.degree('counted', 1::bigint) FROM generate_series(1, 3)")?;
        Spi::run("SELECT warren.degree('counted', 2::bigint)")?;
        let loaded = counters()?.ok_or(Error::InvalidPosition)?;
        assert_eq!(delta(&before, &loaded), [1, 0, 1, 1], "one load, however many calls");
        end_statement();
        Spi::run("SELECT warren.degree('counted', 1::bigint)")?;
        let hit = counters()?.ok_or(Error::InvalidPosition)?;
        assert_eq!(delta(&loaded, &hit), [1, 1, 0, 0], "a hit in the next statement");
        Ok(())
    }

    #[pg_test]
    fn the_backends_row_shows_what_its_cache_holds_and_the_cluster_row_follows() -> Result<(), Error>
    {
        ring("holding", 50, 1)?;
        refresh("holding", true)?;
        Spi::run("SELECT warren.prewarm('holding')")?;
        let row = Spi::get_one::<String>(
            "SELECT format('%s %s %s', private_bytes > 0, transient_bytes, since IS NOT NULL)
             FROM warren.backends() WHERE pid = pg_backend_pid()",
        )?;
        assert_eq!(row.as_deref(), Some("t 0 t"));
        let last = Spi::get_one::<String>(
            "SELECT string_agg(kind, ' ') FROM (SELECT kind FROM warren.backends()) b",
        )?;
        assert!(last.is_some_and(|kinds| kinds.ends_with("cluster")), "the totals come last");
        let cluster = Spi::get_one::<bool>(
            "SELECT lookups >= 1 AND pid IS NULL AND role IS NULL
             FROM warren.backends() WHERE kind = 'cluster'",
        )?;
        assert_eq!(cluster, Some(true));
        Ok(())
    }

    /// Another role sees this backend's row, but not what it holds or
    /// counts, unless it has the privileges of its role or of
    /// `pg_read_all_stats` (DESIGN.md §5.7).
    #[pg_test]
    fn another_roles_backend_shows_only_who_it_is() -> Result<(), Error> {
        Spi::run("CREATE ROLE peeker; CREATE ROLE monitor IN ROLE pg_read_all_stats")?;
        Spi::run("SELECT warren.backends()")?;
        let seen = |role: &str| {
            Spi::get_one::<String>(&format!(
                "SET LOCAL ROLE {role};
                 SELECT format('%s %s', role IS NOT NULL, lookups IS NOT NULL)
                 FROM warren.backends() WHERE pid = pg_backend_pid()"
            ))
        };
        assert_eq!(seen("peeker")?.as_deref(), Some("t f"), "a superuser's backend");
        Spi::run("RESET ROLE")?;
        assert_eq!(seen("monitor")?.as_deref(), Some("t t"), "pg_read_all_stats");
        Spi::run("RESET ROLE")?;
        Ok(())
    }
}
