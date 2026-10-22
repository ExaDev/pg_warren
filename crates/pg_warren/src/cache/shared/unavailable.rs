//! Why the shared cache is unavailable (DESIGN.md §5.6, §14 item 30): where
//! `warren.shared_cache_memory` is set but the shared cache cannot work,
//! every version is loaded into the backend's own cache, as in 0.4.0, and
//! never an error; a `NOTICE`, once per backend, says why. At the default
//! cap (§14 item 32) only a registry of another format is noticed: PG 15
//! and 16 without the preload, and big-endian targets, are configurations
//! the default cannot change, and their every backend would say so.

use std::cell::Cell;

use pgrx::{ereport, PgSqlErrorCode};

use super::setting;

#[cfg(any(feature = "pg17", feature = "pg18", test))]
use super::dsm::FORMAT;

/// Why the shared cache is unavailable to this backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unavailable {
    /// PG 15 or 16 without `shared_preload_libraries = 'pg_warren'`, so no
    /// registry was placed in shared memory.
    #[cfg(any(feature = "pg15", feature = "pg16", test))]
    NotPreloaded,
    /// A big-endian target: a shared version's arrays are in native
    /// layout, and the segments' encoding is little-endian.
    BigEndian,
    /// PG 17 or 18, where the registry was created by a build of pg_warren
    /// with another format (`found`), which this build cannot read: until
    /// a restart drops it, backends of this build load privately.
    #[cfg(any(feature = "pg17", feature = "pg18", test))]
    OtherFormat { found: u32 },
}

thread_local! {
    /// Whether this backend has said why.
    static NOTICED: Cell<bool> = const { Cell::new(false) };
}

impl Unavailable {
    /// The notice, raised once per backend, that versions are loaded
    /// privately, if [`noticed`](Self::noticed) with the cap as it is.
    pub(crate) fn notice(self) {
        if self.noticed(setting::at_default()) && !NOTICED.replace(true) {
            ereport!(NOTICE, PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION, self.message());
        }
    }

    /// Whether it is noticed, the cap `at_default` or set by hand: always
    /// when set, and at the default only for a registry of another format,
    /// which a restart after an upgrade resolves.
    fn noticed(self, at_default: bool) -> bool {
        match self {
            #[cfg(any(feature = "pg17", feature = "pg18", test))]
            Self::OtherFormat { .. } => true,
            _ => !at_default,
        }
    }

    /// Its text.
    fn message(self) -> String {
        let private = "this backend loads graph versions into its own cache";
        match self {
            #[cfg(any(feature = "pg15", feature = "pg16", test))]
            Self::NotPreloaded => format!(
                "warren.shared_cache_memory is set, but on PostgreSQL 15 and 16 the shared \
                 cache needs shared_preload_libraries = 'pg_warren': {private}"
            ),
            Self::BigEndian => format!(
                "warren.shared_cache_memory is set, but the shared cache works only on \
                 little-endian targets: {private}"
            ),
            #[cfg(any(feature = "pg17", feature = "pg18", test))]
            Self::OtherFormat { found } => format!(
                "warren.shared_cache_memory is set, but the shared cache was created by a \
                 build of pg_warren with registry format {found}, not {FORMAT}; restart \
                 PostgreSQL to share graph versions again: {private}"
            ),
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

    #[test]
    fn at_the_default_only_a_registry_of_another_format_is_noticed() {
        assert!(!Unavailable::NotPreloaded.noticed(true), "PG 15 and 16 without the preload");
        assert!(!Unavailable::BigEndian.noticed(true));
        assert!(Unavailable::OtherFormat { found: 7 }.noticed(true), "a restart is needed");
        for unavailable in [
            Unavailable::NotPreloaded,
            Unavailable::BigEndian,
            Unavailable::OtherFormat { found: 7 },
        ] {
            assert!(unavailable.noticed(false), "{unavailable:?}, the cap set by hand");
        }
    }

    #[test]
    fn a_registry_of_another_format_says_to_restart() {
        let message = Unavailable::OtherFormat { found: 7 }.message();
        assert!(message.contains("format 7"), "{message}");
        assert!(message.contains("restart"), "{message}");
        assert!(!message.contains("shared_preload_libraries"), "{message}");
    }
}
