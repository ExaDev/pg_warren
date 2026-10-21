//! The catalog: graph definitions and their versions, as rows of
//! `warren.graphs` and `warren.versions` (DESIGN.md §5.1, §7) — creating
//! graphs, recording and promoting versions, staging and retention.
//!
//! It depends on `access` (catalog statements run as the catalog owner)
//! and `source` (a new graph's queries are checked); `build` and the query
//! features depend on it, never the reverse.

mod bootstrap;
mod create_graph;
mod drop;
mod error;
mod graph;
mod info;
mod promote;
mod readers;
mod retention;
mod version;

pub(crate) use error::CatalogError;
pub(crate) use graph::NewGraph;
pub(crate) use graph::{
    level_graph_name, parse_node_order, require_unreserved, GraphDefinition, GraphSource,
};
pub(crate) use version::{
    compaction_between, current_wal_lsn, insert_version, parent_of, promote_version,
    record_contents, require_current_id_space, require_version, version_record, Lineage,
    NewVersion, VersionId, VersionStats,
};
