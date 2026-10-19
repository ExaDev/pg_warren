//! Catalog adapter: `warren.graphs` / `warren.versions` rows, graph
//! definitions, promotion, staging and retention (DESIGN.md §5.1, §7).

mod bootstrap;
mod create_graph;
mod error;
mod graph;
mod identity;

pub(crate) use error::CatalogError;
pub(crate) use graph::{GraphDefinition, NewGraph};
pub(crate) use identity::{as_extension_owner, as_role, current_user};
