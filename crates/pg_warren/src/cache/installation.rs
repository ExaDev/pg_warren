//! This database's installation of pg_warren (DESIGN.md §5.5, §5.6): the
//! database's oid and pg_warren's `pg_extension` oid, which differs after
//! `DROP EXTENSION` and `CREATE EXTENSION`, when version, vector and graph
//! ids restart. The shared cache's keys name it, and a backend's own caches
//! are dropped when it changes, since their ids would name the new
//! installation's versions and vectors. The extension's oid is looked up
//! once per backend (an index scan of `pg_extension`) and kept until a
//! change to `pg_namespace` is processed: dropping the extension drops its
//! schema, `warren`, so a reinstall always invalidates it, in this backend
//! at its next command and in others at their next transaction.

use std::cell::Cell;

use pgrx::pg_sys;

thread_local! {
    /// pg_warren's `pg_extension` oid, once looked up.
    static EXTENSION: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
    /// Whether the invalidation callback is registered.
    static REGISTERED: Cell<bool> = const { Cell::new(false) };
    /// The extension's oid when `changed` last looked.
    static SEEN: Cell<Option<pg_sys::Oid>> = const { Cell::new(None) };
}

/// This database and pg_warren's `pg_extension` oid in it, if installed.
pub(crate) fn installation() -> Option<(pg_sys::Oid, pg_sys::Oid)> {
    // SAFETY: reads this backend's database, set before any query.
    let database = unsafe { pg_sys::MyDatabaseId };
    Some((database, extension()?))
}

/// Whether pg_warren was installed again since the last call (dropped and
/// created, so another `pg_extension` oid): what this backend cached under
/// the earlier installation must go.
pub(super) fn changed() -> bool {
    let now = extension();
    let before = SEEN.replace(now);
    before.is_some() && now.is_some() && before != now
}

/// pg_warren's `pg_extension` oid, looked up once until invalidated.
fn extension() -> Option<pg_sys::Oid> {
    if let Some(extension) = EXTENSION.get() {
        return Some(extension);
    }
    if !REGISTERED.get() {
        // SAFETY: registers a callback for the rest of the backend's life,
        // on the main thread; it only clears a thread-local.
        unsafe {
            pg_sys::CacheRegisterSyscacheCallback(
                pg_sys::SysCacheIdentifier::NAMESPACEOID as i32,
                Some(forget),
                pg_sys::Datum::from(0),
            );
        }
        REGISTERED.set(true);
    }
    // SAFETY: looks pg_warren up in `pg_extension`, on the main thread
    // inside a transaction.
    let extension = unsafe { pg_sys::get_extension_oid(c"pg_warren".as_ptr(), true) };
    let extension = (extension != pg_sys::Oid::INVALID).then_some(extension);
    EXTENSION.set(extension);
    extension
}

/// Forgets the extension's oid: a namespace changed, perhaps `warren`.
unsafe extern "C-unwind" fn forget(_arg: pg_sys::Datum, _cache: i32, _hash: u32) {
    EXTENSION.set(None);
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::{changed, installation, EXTENSION};

    #[pg_test]
    fn the_extension_is_looked_up_once_until_a_namespace_changes() -> Result<(), Error> {
        let installed = installation();
        assert!(installed.is_some());
        assert!(EXTENSION.get().is_some(), "kept");
        Spi::run("CREATE SCHEMA installation_probe")?;
        assert_eq!(EXTENSION.get(), None, "forgotten at the next command");
        assert_eq!(installation(), installed, "and looked up again");
        Ok(())
    }

    #[pg_test]
    fn the_installation_is_unchanged_by_other_namespaces() -> Result<(), Error> {
        changed();
        assert!(!changed());
        Spi::run("CREATE SCHEMA installation_other")?;
        assert!(!changed(), "the same extension, looked up again");
        Ok(())
    }
}
