//! `warren.reassign_owned`: the SQL counterpart of `REASSIGN OWNED` for
//! graphs and vectors, which no event trigger can see (DESIGN.md §5.7,
//! §14 item 44).

use pgrx::prelude::*;

use super::super::create_graph::{writable_notice, writable_schemas};
use super::super::graph::NEW_OWNER;
use super::super::CatalogError;
use crate::access::{as_catalog_owner, entry, require_can_receive, require_privileges_of};

/// `warren.reassign_owned(old regrole, new regrole)`: gives every graph
/// `old` owns to `new`, with `set_owner`'s rules except its source check
/// (`REASSIGN OWNED` checks none), and makes `new` the creator of every
/// vector `old` wrote. The caller needs the privileges of `old` and must
/// be able to `SET ROLE` to `new`. Returns the number of graphs moved.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."reassign_owned"("old" regrole, "new" regrole) RETURNS bigint
STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'reassign_owned_wrapper';
"#
)]
fn reassign_owned(old: pg_sys::Oid, new: pg_sys::Oid) -> i64 {
    entry(|_| reassign(old, new))
}

/// What a graph's search_path NOTICE needs: its name, whether it is built
/// from queries, and its `search_path`.
type Moved = (String, bool, String);

fn reassign(old: pg_sys::Oid, new: pg_sys::Oid) -> Result<i64, CatalogError> {
    let from = require_privileges_of(old, "reassign its graphs and vectors")?;
    require_can_receive(new, &from)?;
    // One UPDATE takes each row's lock, waiting for a running refresh or
    // set_owner, and re-checks `owner` once it holds it (EvalPlanQual), as
    // `GraphDefinition::lock` re-checks ownership (DESIGN.md §5.7).
    let moved = as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            let graphs = client
                .update(
                    &format!(
                        "UPDATE warren.graphs SET {NEW_OWNER} WHERE owner::oid = $1
                         RETURNING name, source = 'query', search_path"
                    ),
                    None,
                    &[old.into(), new.into()],
                )?
                .map(|row| {
                    Ok::<Moved, pgrx::spi::Error>((
                        row.get(1)?.unwrap_or_default(),
                        row.get(2)?.unwrap_or_default(),
                        row.get(3)?.unwrap_or_default(),
                    ))
                })
                .collect::<Result<Vec<_>, _>>()?;
            client.update(
                "UPDATE warren.vectors SET created_by = $2::oid::regrole WHERE created_by::oid = $1",
                None,
                &[old.into(), new.into()],
            )?;
            Ok::<_, CatalogError>(graphs)
        })
    })?;
    for (graph, built_from_queries, search_path) in &moved {
        if !built_from_queries {
            continue;
        }
        if let Some(schemas) = writable_schemas(search_path, new)? {
            notice!("{}", writable_notice(graph, search_path, &schemas));
        }
    }
    i64::try_from(moved.len()).map_err(|_| CatalogError::TooLarge("graphs reassigned"))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, failure_as, neighbouring_graphs, ownership};

    /// `REASSIGN OWNED` checks no source, so neither does this: `dr_heir`
    /// cannot read `dr`'s tables. Reader entries stay, as privileges do.
    #[pg_test]
    fn reassign_owned_gives_the_new_role_every_graph_and_vector_of_the_old() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run("CREATE ROLE dr_heir; SELECT warren.grant('dr', 'dr_heir')")?;
        let moved = Spi::get_one::<i64>("SELECT warren.reassign_owned('dr_owner', 'dr_heir')")?;
        assert_eq!(moved, Some(1));
        assert_eq!(
            ownership()?.as_deref(),
            Some("dk:dk_owner:{dr_owner} dr:dr_heir:{} / dk.mine:dr_heir dk.theirs:dk_owner")
        );
        Ok(())
    }

    #[pg_test]
    fn reassign_owned_of_a_role_that_owns_nothing_moves_nothing() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run("CREATE ROLE dr_idle; CREATE ROLE dr_heir")?;
        let before = ownership()?;
        let moved = Spi::get_one::<i64>("SELECT warren.reassign_owned('dr_idle', 'dr_heir')")?;
        assert_eq!(moved, Some(0));
        assert_eq!(ownership()?, before);
        Ok(())
    }

    /// As `REASSIGN OWNED`: the privileges of the old role (a member that
    /// does not inherit them has not), and the right to become the new.
    #[pg_test]
    fn reassign_owned_needs_the_privileges_of_both_roles() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run(
            "CREATE ROLE dr_heir; CREATE ROLE dr_mover NOINHERIT; CREATE ROLE dr_helper;
             GRANT dr_heir TO dr_mover; GRANT dr_owner TO dr_mover, dr_helper",
        )?;
        let before = ownership()?;
        let call = "warren.reassign_owned('dr_owner', 'dr_heir')";
        assert_eq!(
            failure_as("dr_mover", call)?.as_deref(),
            Some("42501: must have the privileges of role \"dr_owner\" to reassign its graphs and vectors")
        );
        assert_eq!(
            failure_as("dr_helper", call)?.as_deref(),
            Some(
                "42501: must be able to SET ROLE \"dr_heir\" to give it the graphs and vectors of \
                 role \"dr_owner\""
            )
        );
        assert_eq!(ownership()?, before);
        Ok(())
    }

    #[pg_test]
    fn reassign_owned_of_a_role_that_does_not_exist_is_refused() -> Result<(), Error> {
        let gone = Spi::get_one::<pg_sys::Oid>(
            "CREATE ROLE dr_gone; CREATE ROLE dr_heir; SELECT 'dr_gone'::regrole::oid",
        )?
        .unwrap_or(pg_sys::Oid::INVALID);
        Spi::run("DROP ROLE dr_gone")?;
        assert_eq!(
            failure(&format!("warren.reassign_owned({}::oid::regrole, 'dr_heir')", gone.to_u32()))?,
            Some(format!("42704: role with OID {} does not exist", gone.to_u32()))
        );
        Ok(())
    }
}
