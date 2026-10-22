//! The `Unavailable` adapter (DESIGN.md §5.6, §14 item 30): where
//! `warren.shared_cache_memory` is set but the shared cache cannot work,
//! every version is loaded into the backend's own cache, as in 0.4.0, and
//! never an error; a `NOTICE`, once per backend, says why.

use std::cell::Cell;

use pgrx::{ereport, PgSqlErrorCode};

/// Why the shared cache is unavailable to this backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// PG 15 or 16 without `shared_preload_libraries = 'pg_warren'`, so no
    /// registry was placed in shared memory.
    NotPreloaded,
    /// A big-endian target: a shared version's arrays are in native
    /// layout, and the segments' encoding is little-endian.
    BigEndian,
}

thread_local! {
    /// Whether this backend has said why.
    static NOTICED: Cell<bool> = const { Cell::new(false) };
}

impl Unavailable {
    /// The notice, raised once per backend, that versions are loaded
    /// privately.
    pub(crate) fn notice(self) {
        if !NOTICED.replace(true) {
            ereport!(NOTICE, PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION, self.message());
        }
    }

    /// Its text.
    fn message(self) -> &'static str {
        match self {
            Self::NotPreloaded => {
                "warren.shared_cache_memory is set, but on PostgreSQL 15 and 16 the shared \
                 cache needs shared_preload_libraries = 'pg_warren': this backend loads graph \
                 versions into its own cache"
            }
            Self::BigEndian => {
                "warren.shared_cache_memory is set, but the shared cache works only on \
                 little-endian targets: this backend loads graph versions into its own cache"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_notice_names_the_setting_and_the_remedy() {
        let message = Unavailable::NotPreloaded.message();
        assert!(message.contains("warren.shared_cache_memory"));
        assert!(message.contains("shared_preload_libraries = 'pg_warren'"));
        assert!(Unavailable::BigEndian.message().contains("little-endian"));
    }
}
