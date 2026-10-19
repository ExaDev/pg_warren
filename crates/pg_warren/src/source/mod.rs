//! Graph sources: a graph's `node_query` and `edge_query` (DESIGN.md §5.1,
//! §5.4 steps 1–3) — the result columns they must have, and reading them as
//! read-only SPI cursors under one pinned snapshot — and the id types a
//! graph's node ids may have, with their conversion to and from
//! `warren-core`'s external ids.
//!
//! `catalog` checks the queries when a graph is created; `build` reads them
//! on every refresh. This feature depends on no other feature, and runs
//! queries as whoever is current: switching to the graph owner is the
//! caller's job (`access`).

mod columns;
mod cursor;
mod id_type;

pub(crate) use columns::{check_edge_columns, check_node_columns, ColumnType, SourceError};
pub(crate) use cursor::{for_each_row, open_read_only, source_columns, with_pinned_snapshot};
pub(crate) use id_type::{with_id_type, IdDatum, IdType};
