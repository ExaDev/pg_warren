//! `warren.grant` and `warren.revoke`: the roles, besides members of the
//! owner role, that may query a graph (`graphs.readers`, DESIGN.md §7).
//! Both prune reader roles that no longer exist.

use pgrx::prelude::*;

use super::{CatalogError, GraphDefinition};
use crate::access::require_owner;
use crate::extension::or_raise;

/// `warren.grant(name text, role regrole)`: lets members of `role` query
/// graph `name`. The caller must be a member of the owner role.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."grant"(name text, role regrole) RETURNS void
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'grant_wrapper';
"#
)]
fn grant(name: &str, role: pg_sys::Oid) {
    or_raise(set_reader(name, role, true, "grant"));
}

/// `warren.revoke(name text, role regrole)`: removes `role` from the
/// readers of graph `name`. The caller must be a member of the owner role.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."revoke"(name text, role regrole) RETURNS void
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'revoke_wrapper';
"#
)]
fn revoke(name: &str, role: pg_sys::Oid) {
    or_raise(set_reader(name, role, false, "revoke"));
}

fn set_reader(
    name: &str,
    role: pg_sys::Oid,
    add: bool,
    action: &'static str,
) -> Result<(), CatalogError> {
    let graph = GraphDefinition::find(name)?;
    require_owner(graph.owner, &graph.name, action)?;
    graph.set_reader(role, add)
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::ring;

    fn readers() -> Result<Option<String>, Error> {
        Spi::get_one::<String>("SELECT readers::text FROM warren.graphs WHERE name = 'r'")
    }

    #[pg_test]
    fn grant_adds_a_reader_once_and_revoke_removes_it() -> Result<(), Error> {
        ring("r", 3, 2)?;
        Spi::run(
            "CREATE ROLE reader_a; CREATE ROLE reader_b;
             SELECT warren.grant('r', 'reader_a');
             SELECT warren.grant('r', 'reader_b');
             SELECT warren.grant('r', 'reader_a')",
        )?;
        assert_eq!(readers()?.as_deref(), Some("{reader_a,reader_b}"));
        Spi::run("SELECT warren.revoke('r', 'reader_a')")?;
        assert_eq!(readers()?.as_deref(), Some("{reader_b}"));
        Ok(())
    }

    #[pg_test]
    fn grant_and_revoke_prune_dropped_roles() -> Result<(), Error> {
        ring("r", 3, 2)?;
        Spi::run(
            "CREATE ROLE reader_gone; CREATE ROLE reader_kept;
             SELECT warren.grant('r', 'reader_gone');
             DROP ROLE reader_gone;
             SELECT warren.grant('r', 'reader_kept')",
        )?;
        assert_eq!(readers()?.as_deref(), Some("{reader_kept}"));
        Ok(())
    }

    #[pg_test(error = "must be a member of role \"r_owner\" to grant graph \"r\"")]
    fn only_the_owner_may_grant() -> Result<(), Error> {
        ring("r", 3, 2)?;
        Spi::run("CREATE ROLE granter; SET ROLE granter; SELECT warren.grant('r', 'granter')")
    }
}
