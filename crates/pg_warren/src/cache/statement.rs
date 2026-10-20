//! The statement pin (DESIGN.md §5.5): every version and persisted vector
//! a top-level statement reads or writes stays cached until the statement
//! ends, whatever `warren.max_cache_memory` says, so a statement that makes
//! many `warren.*` calls (a §6.7 library run) loads each once. When the
//! statement ends, normally or with an error, the pin is released and the
//! cache trimmed to the cap.
//!
//! A top-level statement is one client message, the unit of
//! `statement_timestamp()`: Postgres resets `MessageContext` before it
//! reads the next message, including after an error, and a reset callback
//! there ends the pin. Several statements sent in one simple-protocol query
//! string are one message, and so one statement here. Outside a client
//! connection (a background worker), where there is no `MessageContext`,
//! the pin ends with the transaction.

use std::cell::RefCell;
use std::collections::HashSet;

use pgrx::{pg_sys, PgMemoryContexts};

use super::trim;
use crate::catalog::VersionId;
use crate::vector::VectorId;

thread_local! {
    /// What the current top-level statement has pinned.
    static STATEMENT: RefCell<Statement> = RefCell::new(Statement::default());
}

/// The current top-level statement's pins.
#[derive(Default)]
struct Statement {
    /// Whether its end will be signalled: a reset callback is registered.
    open: bool,
    /// The versions it has read.
    versions: HashSet<VersionId>,
    /// The persisted vectors it has read or written.
    vectors: HashSet<VectorId>,
    /// Whether it has raised the notice that a version does not fit.
    noticed: bool,
}

/// Pins version `version` until the statement ends.
pub(super) fn pin_version(version: VersionId) {
    open();
    STATEMENT.with_borrow_mut(|statement| statement.versions.insert(version));
}

/// Pins persisted vector `id` until the statement ends.
pub(super) fn pin_vector(id: VectorId) {
    open();
    STATEMENT.with_borrow_mut(|statement| statement.vectors.insert(id));
}

/// Whether the statement has pinned version `version`.
pub(super) fn version_pinned(version: VersionId) -> bool {
    STATEMENT.with_borrow(|statement| statement.versions.contains(&version))
}

/// Whether the statement has pinned persisted vector `id`.
pub(super) fn vector_pinned(id: VectorId) -> bool {
    STATEMENT.with_borrow(|statement| statement.vectors.contains(&id))
}

/// Whether the notice that a version does not fit the cap is due: once
/// per statement, so a loop over an oversize graph says so once.
pub(super) fn notice_due() -> bool {
    open();
    !STATEMENT.with_borrow_mut(|statement| std::mem::replace(&mut statement.noticed, true))
}

/// Ends the statement: releases its pins and trims the cache to
/// `warren.max_cache_memory`.
pub(crate) fn end_statement() {
    STATEMENT.take();
    trim();
}

/// Ends the statement when dropped: by the reset of the memory context
/// that lives as long as the statement.
struct StatementEnd;

impl Drop for StatementEnd {
    fn drop(&mut self) {
        end_statement();
    }
}

/// Registers the statement's end, once per statement.
fn open() {
    if STATEMENT.with_borrow(|statement| statement.open) {
        return;
    }
    // SAFETY: reads the backend's memory context globals on the main
    // thread; MessageContext is NULL outside a client connection, and a
    // call always runs inside a transaction.
    let context = unsafe {
        match pg_sys::MessageContext.is_null() {
            true => pg_sys::TopTransactionContext,
            false => pg_sys::MessageContext,
        }
    };
    PgMemoryContexts::For(context).leak_and_drop_on_delete(StatementEnd);
    STATEMENT.with_borrow_mut(|statement| statement.open = true);
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{refresh, ring};

    /// `warren.evict(name)`: how many entries of graph `name` the cache
    /// held.
    fn evict(name: &str) -> Result<Option<i64>, Error> {
        Spi::get_one::<i64>(&format!("SELECT warren.evict('{name}')"))
    }

    /// (A `#[pg_test]` is one statement, so its end is called here; that
    /// Postgres ends it, normally or on error, is
    /// `test/sql/cache_statement_pin.sql`.)
    #[pg_test]
    fn a_statement_keeps_what_it_read_past_the_cap_until_it_ends() -> Result<(), Error> {
        for name in ["a", "b"] {
            ring(name, 2000, 1)?;
            refresh(name, true)?;
        }
        Spi::run("SET LOCAL warren.max_cache_memory = '150kB'")?;
        Spi::run("SELECT warren.prewarm('a')")?;
        Spi::run("SELECT warren.prewarm('b')")?;
        assert_eq!(evict("a")?, Some(1), "a was not evicted to make room for b");
        Spi::run("SELECT warren.prewarm('a')")?;
        super::end_statement();
        assert_eq!(evict("b")?, Some(0), "b, the least recently used, went at the end");
        assert_eq!(evict("a")?, Some(1));
        Ok(())
    }

    #[pg_test]
    fn the_unfit_notice_is_due_once_per_statement() {
        assert!(super::notice_due());
        assert!(!super::notice_due());
        super::end_statement();
        assert!(super::notice_due());
    }
}
