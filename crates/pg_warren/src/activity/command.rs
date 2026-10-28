//! `warren.report_progress` (DESIGN.md §5.7, §14 item 49): sets the
//! command frame, the outermost long call of the statement, so that a
//! PL/pgSQL algorithm over the primitives, the library's or a user's own,
//! reports its iterations and levels as native calls report their phases.
//! A command keeps its start while its name stays the same. The frame is
//! cleared when the statement ends, normally or with an error, or its
//! top-level transaction does, by callbacks on `MessageContext` and
//! `TopTransactionContext` like the statement pin's (§5.5): pure Rust, so
//! legal inside `AtCommit_Memory`. A NULL command clears it at once.
//! A loop reports once per iteration, so the graph it names is looked up
//! and its read access checked once per statement and user, not per report.

use std::cell::{Cell, RefCell};

use pgrx::prelude::*;
use pgrx::{pg_sys, PgMemoryContexts};

use super::backend::{claim, record};
use super::frame::{Frame, Name};
use super::operation::now;
use crate::access::{current_user, entry, require_reader, AccessError, SqlError};
use crate::catalog::{CatalogError, GraphDefinition};

thread_local! {
    /// Whether a callback on `MessageContext` is registered.
    static MESSAGE_END: Cell<bool> = const { Cell::new(false) };
    /// Whether a callback on `TopTransactionContext` is registered.
    static TRANSACTION_END: Cell<bool> = const { Cell::new(false) };
    /// The graph the command last reported on, until the frame clears.
    static KNOWN: RefCell<Known> = const { RefCell::new(Known::NONE) };
}

/// The graph a command last reported on, by name and user, once resolved.
#[derive(Debug, Clone, PartialEq)]
struct Known(Option<(String, u32, i32)>);

impl Known {
    /// None resolved.
    const NONE: Self = Self(None);

    /// The id of graph `name` for `user`: the known one, or else what
    /// `resolve` finds, kept if found.
    fn graph_id<E>(
        &mut self,
        name: &str,
        user: u32,
        resolve: impl FnOnce(&str, u32) -> Result<i32, E>,
    ) -> Result<i32, E> {
        if let Some((known, by, id)) = &self.0 {
            if known == name && *by == user {
                return Ok(*id);
            }
        }
        self.0 = None;
        let id = resolve(name, user)?;
        self.0 = Some((name.to_owned(), user, id));
        Ok(id)
    }
}

/// Clears the command frame and the graph it reported on.
fn clear() {
    KNOWN.with_borrow_mut(|known| *known = Known::NONE);
    record(|values| values.command = Frame::NONE);
}

/// What a report sets.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Report<'a> {
    pub(crate) command: &'a str,
    pub(crate) graph: Option<i32>,
    pub(crate) phase: &'a str,
    pub(crate) done: Option<u64>,
    pub(crate) total: Option<u64>,
}

/// The command frame after `report`, replacing `frame`, at `now`: the
/// start is kept while the command's name is the same.
pub(crate) fn reported(frame: &Frame, report: Report<'_>, now: i64) -> Frame {
    let name = Name::new(report.command);
    let start = match frame.is_some() && frame.name == name {
        true => frame.start,
        false => now,
    };
    Frame {
        name,
        graph: report.graph,
        version: None,
        phase: Name::new(report.phase),
        done: report.done,
        total: report.total,
        start,
    }
}

/// Clears the command frame when dropped, by the reset or deletion of the
/// memory context it was registered on, and clears that registration.
/// Pure Rust: it runs inside `AtCommit_Memory` at a transaction's end.
struct CommandEnd(&'static std::thread::LocalKey<Cell<bool>>);

impl Drop for CommandEnd {
    fn drop(&mut self) {
        self.0.set(false);
        clear();
    }
}

/// Registers the ends of the message and of the transaction, once each per
/// life of their contexts.
fn register() {
    // SAFETY: reads the backend's memory context globals on the main
    // thread; MessageContext is NULL outside a client connection, and a
    // call always runs inside a transaction.
    let (message, transaction) = unsafe { (pg_sys::MessageContext, pg_sys::TopTransactionContext) };
    for (registered, context) in [(&MESSAGE_END, message), (&TRANSACTION_END, transaction)] {
        if !context.is_null() && !registered.get() {
            PgMemoryContexts::For(context).leak_and_drop_on_delete(CommandEnd(registered));
            registered.set(true);
        }
    }
}

/// Sets the command frame to `report`, cleared when the statement or its
/// transaction ends: what `warren.report_progress` sets for PL/pgSQL, and
/// what a native algorithm of levels and iterations (`warren.leiden`)
/// sets itself, on a graph whose access it has checked.
pub(crate) fn report_command(report: Report<'_>) {
    claim();
    register();
    let now = now();
    record(|values| values.command = reported(&values.command, report, now));
}

/// The id of graph `name`, which the caller must be able to read.
fn graph_id(name: &str) -> Result<i32, ReportError> {
    let user = current_user().to_u32();
    KNOWN.with_borrow_mut(|known| {
        known.graph_id(name, user, |name, _| {
            let graph = GraphDefinition::find(name)?;
            require_reader(graph.owner, &graph.readers, name, "report progress on")?;
            Ok(graph.graph_id)
        })
    })
}

/// Why a report was refused.
#[derive(Debug, thiserror::Error)]
enum ReportError {
    #[error(transparent)]
    Catalog(#[from] CatalogError),
    #[error(transparent)]
    Access(#[from] AccessError),
    #[error("{0} must not be negative")]
    Negative(&'static str),
}

impl SqlError for ReportError {
    fn sqlstate(&self) -> PgSqlErrorCode {
        match self {
            Self::Catalog(error) => error.sqlstate(),
            Self::Access(error) => error.sqlstate(),
            Self::Negative(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        }
    }
}

/// A unit count, refused if negative.
fn units(value: Option<i64>, name: &'static str) -> Result<Option<u64>, ReportError> {
    value.map(|value| u64::try_from(value).map_err(|_| ReportError::Negative(name))).transpose()
}

/// `warren.report_progress(command, graph, phase, done, total)`: see the
/// module's documentation. `PARALLEL RESTRICTED`: it writes the leader's
/// slot.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
fn report_progress(
    command: Option<&str>,
    graph: Option<&str>,
    phase: Option<&str>,
    done: default!(Option<i64>, "NULL"),
    total: default!(Option<i64>, "NULL"),
) {
    entry(|_| {
        claim();
        let Some(command) = command else {
            clear();
            return Ok(());
        };
        let (done, total) = (units(done, "done")?, units(total, "total")?);
        // Before the graph is resolved and remembered: whatever the call
        // remembers is forgotten when the statement ends.
        register();
        let graph = graph.map(graph_id).transpose()?;
        report_command(Report { command, graph, phase: phase.unwrap_or_default(), done, total });
        Ok::<_, ReportError>(())
    })
}

#[cfg(test)]
mod reports {
    use super::*;

    fn report(command: &str) -> Report<'_> {
        Report { command, graph: Some(3), phase: "level 1", done: Some(2), total: None }
    }

    #[test]
    fn a_graph_is_resolved_once_per_name_and_user_until_forgotten() {
        let mut known = Known::NONE;
        let mut resolved = Vec::new();
        let mut resolve = |name: &str, user: u32| {
            resolved.push((name.to_owned(), user));
            Ok::<_, ()>(7)
        };
        assert_eq!(known.graph_id("g", 1, &mut resolve), Ok(7));
        assert_eq!(known.graph_id("g", 1, &mut resolve), Ok(7));
        assert_eq!(known.graph_id("g", 2, &mut resolve), Ok(7));
        assert_eq!(known.graph_id("h", 2, &mut resolve), Ok(7));
        known = Known::NONE;
        assert_eq!(known.graph_id("h", 2, &mut resolve), Ok(7));
        let failing = |_: &str, _: u32| Err::<i32, _>("refused");
        assert_eq!(known.graph_id("refused", 2, failing), Err("refused"));
        assert_eq!(
            known.graph_id("h", 2, |_, _| Err("again")),
            Err("again"),
            "a refusal is not kept"
        );
        assert_eq!(
            resolved,
            [("g".to_owned(), 1), ("g".to_owned(), 2), ("h".to_owned(), 2), ("h".to_owned(), 2)]
        );
    }

    #[test]
    fn a_command_keeps_its_start_while_its_name_stays_the_same() {
        let first = reported(&Frame::NONE, report("louvain"), 100);
        assert_eq!((first.name.as_str(), first.start), ("louvain", 100));
        assert_eq!((first.phase.as_str(), first.done, first.total), ("level 1", Some(2), None));
        let again = reported(&first, Report { phase: "level 2", ..report("louvain") }, 200);
        assert_eq!((again.phase.as_str(), again.start), ("level 2", 100));
        let other = reported(&again, report("leiden"), 300);
        assert_eq!(other.start, 300);
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, failure_as, ring};

    /// This backend's row of `warren.progress()`, as
    /// `command graph phase done total operation started`.
    fn own_row() -> Result<Option<String>, Error> {
        Spi::get_one::<String>(
            "SELECT (SELECT concat_ws(' ', command, coalesce(graph, '-'), coalesce(phase, '-'),
                                      coalesce(done::text, '-'), coalesce(total::text, '-'),
                                      coalesce(operation, '-'), command_start IS NOT NULL)
                     FROM warren.progress() WHERE pid = pg_backend_pid())",
        )
    }

    #[pg_test]
    fn a_report_sets_the_command_of_this_backends_row() -> Result<(), Error> {
        ring("reported", 10, 1)?;
        Spi::run("SELECT warren.report_progress('louvain', 'reported', 'level 1', 2, 10)")?;
        assert_eq!(own_row()?.as_deref(), Some("louvain reported level 1 2 10 - t"));
        Spi::run("SELECT warren.report_progress('louvain', NULL, 'level 2')")?;
        assert_eq!(own_row()?.as_deref(), Some("louvain - level 2 - - - t"));
        Spi::run("SELECT warren.report_progress(NULL, NULL, NULL)")?;
        assert_eq!(own_row()?, None, "a NULL command clears it");
        Ok(())
    }

    #[pg_test]
    fn a_report_needs_a_readable_graph_and_counts_of_at_least_zero() -> Result<(), Error> {
        ring("guarded", 10, 1)?;
        let state = |expression: &str| -> Result<Option<String>, Error> {
            Ok(failure(expression)?.map(|failed| failed[..5].to_owned()))
        };
        assert_eq!(
            state("warren.report_progress('c', 'guarded', 'p', -1)")?.as_deref(),
            Some("22023")
        );
        assert_eq!(
            state("warren.report_progress('c', 'guarded', 'p', 1, -1)")?.as_deref(),
            Some("22023")
        );
        assert_eq!(state("warren.report_progress('c', 'missing', 'p')")?.as_deref(), Some("42704"));
        Spi::run("CREATE ROLE outsider")?;
        let refused = failure_as("outsider", "warren.report_progress('c', 'guarded', 'p')")?;
        assert_eq!(refused.map(|failed| failed[..5].to_owned()).as_deref(), Some("42501"));
        Ok(())
    }

    #[pg_test]
    fn another_role_sees_what_a_backend_runs_but_not_how_far() -> Result<(), Error> {
        ring("watched", 10, 1)?;
        Spi::run("SELECT warren.report_progress('mine', 'watched', 'secret phase', 1, 2)")?;
        Spi::run("CREATE ROLE onlooker; CREATE ROLE overseer IN ROLE pg_read_all_stats")?;
        let seen = |role: &str| -> Result<Option<String>, Error> {
            Spi::run(&format!("SET ROLE {role}"))?;
            let row = own_row();
            Spi::run("RESET ROLE")?;
            row
        };
        assert_eq!(seen("onlooker")?.as_deref(), Some("mine - - - - - t"));
        assert_eq!(
            seen("overseer")?.as_deref(),
            Some("mine - secret phase 1 2 - t"),
            "how far, but not the name of a graph it may not read"
        );
        Ok(())
    }
}
