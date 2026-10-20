//! The statement pin (DESIGN.md §5.5): every version and persisted vector
//! a top-level statement reads or writes stays cached until the statement
//! ends, or its transaction does, whichever comes first, so a statement
//! that makes many `warren.*` calls (a §6.7 library run) loads each once.
//! A pinned entry that no call is using gives way only under pressure:
//! when a load or a call's compute budget needs the room (`bound`). When
//! the pin ends, normally or with an error, the cache is trimmed to
//! `warren.max_cache_memory`.
//!
//! A top-level statement is one client message, the unit of
//! `statement_timestamp()`: Postgres resets `MessageContext` before it
//! reads the next message, including after an error, and a reset callback
//! there ends the pin. So with the extended protocol each `Execute` is a
//! statement: a portal fetched over several `Execute`s is pinned only
//! during each. Several statements sent in one simple-protocol query string
//! are one message, and so one statement here. A callback on
//! `TopTransactionContext` ends the pin at the end of the top-level
//! transaction too, so a `CALL` (or `DO`) that commits is pinned only
//! within each of its transactions, and a background worker, which has no
//! `MessageContext`, is pinned per transaction.
//!
//! Each callback is registered at most once per life of its context and
//! ends whatever pin is open when it fires, which can only be one taken
//! since: a `MessageContext` reset ends the message, and a
//! `TopTransactionContext` reset or deletion ends the transaction, inside
//! which any open pin began. No callback can end a later statement's pin.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;

use pgrx::{pg_sys, PgMemoryContexts};

use super::trim;
use crate::catalog::VersionId;
use crate::vector::VectorId;

thread_local! {
    /// What the current pin holds.
    static STATEMENT: RefCell<Statement> = RefCell::new(Statement::default());
    /// Whether a callback on `MessageContext` is registered.
    static MESSAGE_END: Cell<bool> = const { Cell::new(false) };
    /// Whether a callback on `TopTransactionContext` is registered.
    static TRANSACTION_END: Cell<bool> = const { Cell::new(false) };
}

/// The current pin.
#[derive(Default)]
struct Statement {
    /// The versions read.
    versions: HashSet<VersionId>,
    /// The persisted vectors read or written.
    vectors: HashSet<VectorId>,
    /// Whether the notice that a version does not fit has been raised.
    noticed: bool,
}

/// Pins version `version` until the statement or transaction ends.
pub(super) fn pin_version(version: VersionId) {
    register();
    STATEMENT.with_borrow_mut(|statement| statement.versions.insert(version));
}

/// Pins persisted vector `id` until the statement or transaction ends.
pub(super) fn pin_vector(id: VectorId) {
    register();
    STATEMENT.with_borrow_mut(|statement| statement.vectors.insert(id));
}

/// Whether version `version` is pinned.
pub(super) fn version_pinned(version: VersionId) -> bool {
    STATEMENT.with_borrow(|statement| statement.versions.contains(&version))
}

/// Whether persisted vector `id` is pinned.
pub(super) fn vector_pinned(id: VectorId) -> bool {
    STATEMENT.with_borrow(|statement| statement.vectors.contains(&id))
}

/// Whether the notice that a version does not fit the cap is due: once
/// per pin, so a loop over an oversize graph says so once.
pub(super) fn notice_due() -> bool {
    register();
    !STATEMENT.with_borrow_mut(|statement| std::mem::replace(&mut statement.noticed, true))
}

/// Ends the pin: releases what it holds and trims the cache to
/// `warren.max_cache_memory`.
pub(crate) fn end_statement() {
    STATEMENT.take();
    trim();
}

/// Ends the pin when dropped, by the reset or deletion of the memory
/// context it was registered on, and clears that context's registration.
///
/// It must never call into Postgres and never panic: at the end of every
/// transaction that pinned, in any backend, it runs inside
/// `AtCommit_Memory`, after the commit record is written, where an ERROR is
/// promoted to PANIC. `end_statement` and `trim` are pure
/// Rust over this backend's own maps.
struct StatementEnd(&'static std::thread::LocalKey<Cell<bool>>);

impl Drop for StatementEnd {
    fn drop(&mut self) {
        self.0.set(false);
        end_statement();
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
            PgMemoryContexts::For(context).leak_and_drop_on_delete(StatementEnd(registered));
            registered.set(true);
        }
    }
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
