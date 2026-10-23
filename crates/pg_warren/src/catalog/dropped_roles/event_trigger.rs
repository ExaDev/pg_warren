//! `warren.on_drop_owned()`, the optional event trigger on `DROP OWNED`
//! (DESIGN.md §5.7, §14 item 44, decided a: refuse).

use std::ptr;

use pgrx::prelude::*;
use pgrx::PgList;

use super::super::CatalogError;
use super::clean_up_after;
use crate::access::{as_catalog_owner, entry, has_privileges_of, role_name};
use crate::read_only::read_one;

/// `warren.on_drop_owned()`: for `CREATE EVENT TRIGGER … ON
/// ddl_command_start WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION
/// warren.on_drop_owned()`, which a superuser may create and the extension
/// never does. It refuses the `DROP OWNED` (2BP01) while a role it names
/// owns graphs, naming them; otherwise it removes the roles' reader entries
/// and the vectors they wrote, as `warren.drop_owned` does. pgrx has no
/// `event_trigger` type, so the function takes the raw call info.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren."on_drop_owned"() RETURNS event_trigger
VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'on_drop_owned_wrapper';
"#
)]
fn on_drop_owned(fcinfo: pg_sys::FunctionCallInfo) {
    entry(|_| refuse_or_clean_up(&named_roles(fcinfo)?));
}

/// The roles named by the `DROP OWNED` that fired the trigger, without
/// those that do not exist or whose privileges the caller lacks: `DROP
/// OWNED` checks both only after `ddl_command_start`, and raises its own
/// errors for them. Each role once, in the statement's order; none for any
/// other command.
fn named_roles(fcinfo: pg_sys::FunctionCallInfo) -> Result<Vec<pg_sys::Oid>, CatalogError> {
    // SAFETY: Postgres passes this call's own call info, valid for the call.
    let context = unsafe { fcinfo.as_ref() }.map_or(ptr::null_mut(), |info| info.context);
    // SAFETY: `context` is NULL or a node, whose tag `is_a` reads; Postgres
    // does not stop a direct call, which has no event trigger data.
    if context.is_null() || !unsafe { pgrx::is_a(context, pg_sys::NodeTag::T_EventTriggerData) } {
        return Err(CatalogError::NotEventTrigger);
    }
    // SAFETY: an `EventTriggerData` (checked above), whose parse tree is the
    // running command's node.
    let tree = unsafe { (*context.cast::<pg_sys::EventTriggerData>()).parsetree };
    // SAFETY: as above, for the parse tree.
    if tree.is_null() || !unsafe { pgrx::is_a(tree, pg_sys::NodeTag::T_DropOwnedStmt) } {
        return Ok(Vec::new());
    }
    // SAFETY: a `DropOwnedStmt` (checked above): its roles are a `List` of
    // `RoleSpec`, the same on PG 15–18.
    let specs = unsafe {
        PgList::<pg_sys::RoleSpec>::from_pg((*tree.cast::<pg_sys::DropOwnedStmt>()).roles)
    };
    let mut roles = Vec::new();
    for spec in specs.iter_ptr() {
        // SAFETY: a `RoleSpec` of the statement. With `missing_ok`, a role
        // that does not exist is `InvalidOid`; `PUBLIC`, which `DROP OWNED`
        // rejects in its grammar, would still raise (an ERROR, unwound).
        let role = unsafe { pg_sys::get_rolespec_oid(spec, true) };
        if role != pg_sys::Oid::INVALID && has_privileges_of(role) && !roles.contains(&role) {
            roles.push(role);
        }
    }
    Ok(roles)
}

/// Refuses while one of `roles` owns graphs (DESIGN.md §14 item 44a), and
/// otherwise removes the rest of what they hold.
fn refuse_or_clean_up(roles: &[pg_sys::Oid]) -> Result<(), CatalogError> {
    let owned = roles
        .iter()
        .map(|&role| Ok((role_name(role).unwrap_or_default(), graphs_owned_by(role)?)))
        .collect::<Result<Vec<_>, CatalogError>>()?;
    if let Some(message) = refusal(&owned) {
        return Err(CatalogError::OwnsGraphs(message));
    }
    as_catalog_owner(|| {
        Spi::connect_mut(|client| {
            for &role in roles {
                clean_up_after(client, role)?;
            }
            Ok::<_, CatalogError>(())
        })
    })
}

/// The names of the graphs `role` owns, in order.
fn graphs_owned_by(role: pg_sys::Oid) -> Result<Vec<String>, CatalogError> {
    as_catalog_owner(|| {
        Ok(read_one::<Vec<String>>(
            "SELECT coalesce(array_agg(name ORDER BY name), '{}') FROM warren.graphs
             WHERE owner::oid = $1",
            &[role.into()],
        )?
        .unwrap_or_default())
    })
}

/// The refusal's message, naming each role of `owned` that owns graphs,
/// with its graphs, and the two functions that move or drop them; `None`
/// if none owns any.
fn refusal(owned: &[(String, Vec<String>)]) -> Option<String> {
    let owners: Vec<String> = owned
        .iter()
        .filter(|(_, graphs)| !graphs.is_empty())
        .map(|(role, graphs)| {
            let noun = if graphs.len() == 1 { "graph" } else { "graphs" };
            let names: Vec<String> = graphs.iter().map(|graph| format!("\"{graph}\"")).collect();
            format!("by role \"{role}\" while it owns {noun} {}", names.join(", "))
        })
        .collect();
    if owners.is_empty() {
        return None;
    }
    let graphs: usize = owned.iter().map(|(_, graphs)| graphs.len()).sum();
    let them = if graphs == 1 { "it" } else { "them" };
    Some(format!(
        "cannot drop objects owned {}; give {them} a new owner with \
         warren.reassign_owned(old, new) or drop {them} with warren.drop_owned(role) first",
        owners.join(", nor ")
    ))
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, neighbouring_graphs, ownership, quotas, statement_failure_as};

    /// The trigger as `docs/semantics.md` tells a superuser to create it.
    fn install() -> Result<(), Error> {
        Spi::run(
            "CREATE EVENT TRIGGER warren_drop_owned ON ddl_command_start
             WHEN TAG IN ('DROP OWNED') EXECUTE FUNCTION warren.on_drop_owned()",
        )
    }

    #[pg_test]
    fn a_direct_call_is_refused() -> Result<(), Error> {
        assert_eq!(
            failure("warren.on_drop_owned()")?.as_deref(),
            Some("39P03: warren.on_drop_owned() may be called only by an event trigger")
        );
        Ok(())
    }

    /// DESIGN.md §14 item 44a: the standard procedure, having run
    /// `REASSIGN OWNED` (which moves no graph), is stopped, not surprised.
    #[pg_test]
    fn drop_owned_is_refused_while_a_role_it_names_owns_graphs() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run("CREATE ROLE dr_heir; REASSIGN OWNED BY dr_owner TO dr_heir")?;
        install()?;
        let before = ownership()?;
        assert_eq!(
            statement_failure_as("none", "DROP OWNED BY dr_owner, dk_owner")?.as_deref(),
            Some(
                "2BP01: cannot drop objects owned by role \"dr_owner\" while it owns graph \"dr\", \
                 nor by role \"dk_owner\" while it owns graph \"dk\"; give them a new owner with \
                 warren.reassign_owned(old, new) or drop them with warren.drop_owned(role) first"
            )
        );
        assert_eq!(ownership()?, before);
        Ok(())
    }

    #[pg_test]
    fn a_role_named_twice_is_named_once() -> Result<(), Error> {
        neighbouring_graphs()?;
        install()?;
        assert_eq!(
            statement_failure_as("none", "DROP OWNED BY dr_owner, CURRENT_USER, dr_owner")?
                .as_deref(),
            Some(
                "2BP01: cannot drop objects owned by role \"dr_owner\" while it owns graph \"dr\"; \
                 give it a new owner with warren.reassign_owned(old, new) or drop it with \
                 warren.drop_owned(role) first"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn after_reassign_owned_drop_owned_removes_the_roles_reader_entries_and_quotas(
    ) -> Result<(), Error> {
        neighbouring_graphs()?;
        install()?;
        Spi::run(
            "CREATE ROLE dr_heir;
             SELECT warren.reassign_owned('dr_owner', 'dr_heir');
             DROP OWNED BY dr_owner",
        )?;
        assert_eq!(
            ownership()?.as_deref(),
            Some("dk:dk_owner:{} dr:dr_heir:{} / dk.mine:dr_heir dk.theirs:dk_owner")
        );
        assert_eq!(quotas()?.as_deref(), Some(""));
        Ok(())
    }

    #[pg_test]
    fn drop_owned_of_a_role_that_owns_no_graph_removes_its_reader_entries_and_vectors(
    ) -> Result<(), Error> {
        neighbouring_graphs()?;
        install()?;
        Spi::run(
            "CREATE ROLE dk_reader; SELECT warren.grant('dk', 'dk_reader');
             SET ROLE dk_reader; SELECT warren.vector_const('dk', 'its', 3); RESET ROLE;
             DROP OWNED BY dk_reader",
        )?;
        assert_eq!(
            ownership()?.as_deref(),
            Some("dk:dk_owner:{dr_owner} dr:dr_owner:{} / dk.mine:dr_owner dk.theirs:dk_owner")
        );
        Ok(())
    }

    /// `DROP OWNED` checks privileges after `ddl_command_start`, so the
    /// trigger leaves a role the caller lacks the privileges of, unread, to
    /// `DROP OWNED`'s own refusal; and a role that does not exist to its
    /// own error.
    #[pg_test]
    fn roles_the_caller_may_not_drop_for_are_left_to_drop_owned() -> Result<(), Error> {
        neighbouring_graphs()?;
        install()?;
        Spi::run("CREATE ROLE dr_stranger")?;
        assert_eq!(
            statement_failure_as("dr_stranger", "DROP OWNED BY dr_owner")?.as_deref(),
            Some("42501: permission denied to drop objects")
        );
        assert_eq!(
            statement_failure_as("none", "DROP OWNED BY dr_nobody")?.as_deref(),
            Some("42704: role \"dr_nobody\" does not exist")
        );
        Ok(())
    }

    /// Only `DROP OWNED` has roles to clean up after.
    #[pg_test]
    fn other_commands_pass_through() -> Result<(), Error> {
        neighbouring_graphs()?;
        Spi::run(
            "CREATE EVENT TRIGGER warren_everything ON ddl_command_start
             EXECUTE FUNCTION warren.on_drop_owned();
             CREATE TABLE dr_more (id int); DROP TABLE dr_more",
        )
    }
}

/// The refusal's message, which needs no database.
#[cfg(test)]
mod refusal_message {
    use super::refusal;

    fn owning(role: &str, graphs: &[&str]) -> (String, Vec<String>) {
        (role.to_owned(), graphs.iter().map(|&graph| graph.to_owned()).collect())
    }

    #[test]
    fn the_refusal_names_every_role_that_owns_graphs_and_only_those() {
        assert_eq!(refusal(&[owning("a", &[]), owning("b", &[])]), None);
        assert_eq!(
            refusal(&[owning("a", &["g", "h"]), owning("b", &[]), owning("c", &["k"])]).as_deref(),
            Some(
                "cannot drop objects owned by role \"a\" while it owns graphs \"g\", \"h\", nor by \
                 role \"c\" while it owns graph \"k\"; give them a new owner with \
                 warren.reassign_owned(old, new) or drop them with warren.drop_owned(role) first"
            )
        );
        assert_eq!(
            refusal(&[owning("a", &["g"])]).as_deref(),
            Some(
                "cannot drop objects owned by role \"a\" while it owns graph \"g\"; give it a new \
                 owner with warren.reassign_owned(old, new) or drop it with \
                 warren.drop_owned(role) first"
            )
        );
    }
}
