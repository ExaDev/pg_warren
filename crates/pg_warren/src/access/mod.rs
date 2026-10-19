//! Access: who a statement runs as, and who may call what (DESIGN.md §7).
//!
//! Every `warren.*` function is `SECURITY INVOKER`. This feature owns the
//! two identity switches (to the graph owner for source queries, to the
//! extension owner for statements against the `warren.*` tables) and the
//! membership checks that management and query functions make. It depends
//! on no other feature; the catalog, build and query features depend on it.

mod error;
mod guard;
mod identity;
mod privilege;

pub(crate) use error::AccessError;
pub(crate) use guard::{entry, with_read_only, with_search_path, Caller, SqlError};
pub(crate) use identity::{as_extension_owner, as_role, current_user};
pub(crate) use privilege::{is_superuser, require_creator_or_owner, require_owner, require_reader};
