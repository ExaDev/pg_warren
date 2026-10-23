//! Who sees what of other backends' activity (DESIGN.md §5.7, §14 item 49),
//! by `pg_stat_activity`'s rule: every row shows its pid, database, role and
//! start times; the rest only where the caller has the privileges of the
//! backend's role, or of `pg_read_all_stats`. Totals are everyone's.

use pgrx::pg_sys::{self, Oid};

/// Whether a caller sees the details of a backend whose role is `role`,
/// where `has_privileges_of` answers for the caller.
pub(super) fn details_shown(role: Oid, has_privileges_of: impl Fn(Oid) -> bool) -> bool {
    has_privileges_of(role) || has_privileges_of(Oid::from(pg_sys::ROLE_PG_READ_ALL_STATS))
}

/// [`details_shown`] for the current user.
pub(super) fn shown_to_caller(role: Oid) -> bool {
    details_shown(role, crate::access::has_privileges_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: Oid = Oid::from_u32(20_001);
    const BOB: Oid = Oid::from_u32(20_002);

    #[test]
    fn a_backends_details_need_the_privileges_of_its_role_or_of_pg_read_all_stats() {
        let alice = |role: Oid| role == ALICE;
        assert!(details_shown(ALICE, alice), "alice's own backend");
        assert!(!details_shown(BOB, alice), "bob's, to alice");
        let monitor = |role: Oid| role == Oid::from(pg_sys::ROLE_PG_READ_ALL_STATS);
        assert!(details_shown(BOB, monitor), "anyone's, to a member of pg_read_all_stats");
        let superuser = |_: Oid| true;
        assert!(details_shown(BOB, superuser));
    }
}
