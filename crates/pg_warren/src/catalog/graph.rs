//! Graph definitions: the rows of `warren.graphs` (DESIGN.md §5.1), and the
//! owner check that management functions make (DESIGN.md §7).

use pgrx::prelude::*;

use super::{as_extension_owner, CatalogError};

/// A graph's definition, as `warren.create_graph` recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphDefinition {
    /// `graphs.graph_id`.
    pub(crate) graph_id: i32,
    /// `graphs.name`.
    pub(crate) name: String,
    /// The owner role: source queries run as this role (DESIGN.md §7).
    pub(crate) owner: pg_sys::Oid,
    /// `SELECT <id> ...`.
    pub(crate) node_query: String,
    /// `SELECT <src>, <dst> [, <rel>] [, <weight>] ...`.
    pub(crate) edge_query: String,
    /// Whether edges are directed; undirected edges are stored both ways.
    pub(crate) directed: bool,
    /// Whether the edge query has a weight column.
    pub(crate) weighted: bool,
    /// Whether the edge query has a relation column.
    pub(crate) has_relations: bool,
    /// Whether to build the reverse CSRs.
    pub(crate) build_reverse: bool,
    /// The promoted version builds start from, if any.
    pub(crate) current_version: Option<i64>,
}

/// A graph definition to record: the arguments of `warren.create_graph`,
/// plus the owner and the id type it determined.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewGraph<'a> {
    pub(crate) name: &'a str,
    pub(crate) owner: pg_sys::Oid,
    pub(crate) node_query: &'a str,
    pub(crate) edge_query: &'a str,
    pub(crate) id_type: pg_sys::Oid,
    pub(crate) directed: bool,
    pub(crate) weighted: bool,
    pub(crate) has_relations: bool,
    pub(crate) build_reverse: bool,
    pub(crate) retain_versions: i32,
}

impl GraphDefinition {
    /// The definition of graph `name`.
    pub(crate) fn find(name: &str) -> Result<Self, CatalogError> {
        as_extension_owner(|| {
            Spi::connect(|client| {
                let row = client
                    .select(
                        "SELECT graph_id, owner::oid, node_query, edge_query, directed,
                                weighted, has_relations, build_reverse, current_version
                         FROM warren.graphs WHERE name = $1",
                        Some(1),
                        &[name.into()],
                    )?
                    .first();
                if row.is_empty() {
                    return Err(CatalogError::GraphNotFound(name.to_owned()));
                }
                Ok(Self {
                    graph_id: row.get(1)?.unwrap_or_default(),
                    name: name.to_owned(),
                    owner: row.get(2)?.unwrap_or(pg_sys::Oid::INVALID),
                    node_query: row.get(3)?.unwrap_or_default(),
                    edge_query: row.get(4)?.unwrap_or_default(),
                    directed: row.get(5)?.unwrap_or_default(),
                    weighted: row.get(6)?.unwrap_or_default(),
                    has_relations: row.get(7)?.unwrap_or_default(),
                    build_reverse: row.get(8)?.unwrap_or_default(),
                    current_version: row.get(9)?,
                })
            })
        })
    }

    /// Checks that the caller is a member of the owner role, as management
    /// functions require; `action` names the function for the error.
    pub(crate) fn check_owner(&self, action: &'static str) -> Result<(), CatalogError> {
        let (owner, member) = Spi::get_two_with_args::<String, bool>(
            "SELECT r.rolname::text,
                    CASE WHEN r.oid IS NOT NULL THEN pg_has_role(r.oid, 'MEMBER') END
             FROM (SELECT $1::oid AS oid) o LEFT JOIN pg_roles r ON r.oid = o.oid",
            &[self.owner.into()],
        )?;
        match (owner, member) {
            (Some(_), Some(true)) => Ok(()),
            (Some(owner), _) => {
                Err(CatalogError::NotOwner { owner, action, graph: self.name.clone() })
            }
            (None, _) => Err(CatalogError::OwnerDropped(self.name.clone())),
        }
    }

    /// Locks the graph's row until the end of the transaction, so that
    /// refreshes of one graph run one at a time, and re-reads its current
    /// version, which a refresh that held the lock may have changed.
    pub(crate) fn lock(&mut self) -> Result<(), CatalogError> {
        self.current_version = as_extension_owner(|| {
            Ok::<_, CatalogError>(Spi::get_one_with_args::<i64>(
                "SELECT current_version FROM warren.graphs WHERE graph_id = $1 FOR UPDATE",
                &[self.graph_id.into()],
            )?)
        })?;
        Ok(())
    }
}

impl NewGraph<'_> {
    /// Records a new graph definition.
    pub(crate) fn create(&self) -> Result<(), CatalogError> {
        let graph = self;
        as_extension_owner(|| {
            Spi::connect_mut(|client| {
                let exists = client
                    .select(
                        "SELECT 1 FROM warren.graphs WHERE name = $1",
                        Some(1),
                        &[graph.name.into()],
                    )?
                    .len();
                if exists > 0 {
                    return Err(CatalogError::GraphExists(graph.name.to_owned()));
                }
                client.update(
                    "INSERT INTO warren.graphs (name, owner, node_query, edge_query, id_type,
                         directed, weighted, has_relations, build_reverse, retain_versions)
                     VALUES ($1, $2::oid::regrole, $3, $4, $5::oid::regtype, $6, $7, $8, $9, $10)",
                    None,
                    &[
                        graph.name.into(),
                        graph.owner.into(),
                        graph.node_query.into(),
                        graph.edge_query.into(),
                        graph.id_type.into(),
                        graph.directed.into(),
                        graph.weighted.into(),
                        graph.has_relations.into(),
                        graph.build_reverse.into(),
                        graph.retain_versions.into(),
                    ],
                )?;
                Ok(())
            })
        })
    }
}
