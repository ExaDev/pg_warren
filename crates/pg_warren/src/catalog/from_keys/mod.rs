//! Graphs from keys: the node and edge queries that a list of tables'
//! primary and foreign keys give, so that a graph over them needs no
//! hand-written SQL (DESIGN.md §9.3, §14 item 53). Each listed table's
//! rows are nodes, each foreign key between listed tables a relation, and
//! a junction table (its primary key two foreign keys, and nothing more)
//! an edge per row. The queries are only generated: graphs are built from
//! them as from any `create_graph`'s.

mod error;
mod keys;
mod queries;
mod quote;
mod read;
mod sql;

pub(crate) use error::KeysError;
