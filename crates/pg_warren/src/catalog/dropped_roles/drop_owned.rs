//! `warren.drop_owned`: the SQL counterpart of `DROP OWNED` for graphs and
//! vectors (DESIGN.md §5.7).

use pgrx::prelude::*;

use super::super::CatalogError;
use super::clean_up_after;
use crate::access::{as_catalog_owner, entry, require_privileges_of};

/// `warren.drop_owned(role regrole)`: drops every graph `role` owns (with
/// its versions and everyone's vectors, as `drop_graph` does), deletes the
/// vectors it wrote on other graphs and removes it from every graph's
/// readers. The caller needs the privileges of `role`. Returns the number
/// of graphs dropped.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."drop_owned"(role regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'drop_owned_wrapper';
"#
)]
fn drop_owned(role: pg_sys::Oid) -> i64 {
    entry(|_| drop_everything_of(role))
}

fn drop_everything_of(role: pg_sys::Oid) -> Result<i64, CatalogError> {
    require_privileges_of(role, "drop its graphs and vectors")?;
    let dropped = as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            // Deleting a graph's row takes its lock, waiting for a running
            // refresh or set_owner, and re-checks `owner` once it holds it
            // (EvalPlanQual), as `drop_graph`'s lock does (DESIGN.md §5.7).
            let dropped = client
                .update(
                    "DELETE FROM warren.graphs WHERE owner::oid = $1 RETURNING graph_id",
                    None,
                    &[role.into()],
                )?
                .len();
            clean_up_after(client, role)?;
            Ok::<_, CatalogError>(dropped)
        })
    })?;
    i64::try_from(dropped).map_err(|_| CatalogError::TooLarge("graphs dropped"))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, failure_as, neighbouring_graphs, ownership};

    #[pg_test]
    fn drop_owned_drops_the_roles_graphs_its_vectors_and_its_reader_entries() -> Result<(), Error> {
        neighbouring_graphs()?;
        let dropped = Spi::get_one::<i64>("SELECT warren.drop_owned('dr_owner')")?;
        assert_eq!(dropped, Some(1));
        assert_eq!(ownership()?.as_deref(), Some("dk:dk_owner:{} / dk.theirs:dk_owner"));
        Ok(())
    }

    /// As `DROP OWNED`: the privileges of the role, which a member that
    /// does not inherit them has not.
    #[pg_test]
    fn drop_owned_needs_the_privileges_of_the_role() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run(
            "CREATE ROLE dr_stranger; CREATE ROLE dr_member NOINHERIT;
             GRANT dr_owner TO dr_member",
        )?;
        let before = ownership()?;
        for role in ["dr_stranger", "dr_member"] {
            assert_eq!(
                failure_as(role, "warren.drop_owned('dr_owner')")?.as_deref(),
                Some(
                    "42501: must have the privileges of role \"dr_owner\" to drop its graphs and \
                     vectors"
                ),
                "{role}"
            );
        }
        assert_eq!(ownership()?, before);
        Spi::run("GRANT dr_owner TO dr_stranger")?;
        assert_eq!(failure_as("dr_stranger", "warren.drop_owned('dr_owner')")?, None);
        Ok(())
    }

    #[pg_test]
    fn drop_owned_of_a_role_that_does_not_exist_is_refused() -> Result<(), Error> {
        let gone =
            Spi::get_one::<pg_sys::Oid>("CREATE ROLE dr_gone; SELECT 'dr_gone'::regrole::oid")?
                .unwrap_or(pg_sys::Oid::INVALID);
        Spi::run("DROP ROLE dr_gone")?;
        assert_eq!(
            failure(&format!("warren.drop_owned({}::oid::regrole)", gone.to_u32()))?,
            Some(format!("42704: role with OID {} does not exist", gone.to_u32()))
        );
        Ok(())
    }
}
