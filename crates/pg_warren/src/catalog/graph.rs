//! Graph definitions: the rows of `warren.graphs` (DESIGN.md §5.1).

use std::cell::OnceCell;

use pgrx::prelude::*;
use pgrx::spi::OwnedPreparedStatement;
use pgrx::PgOid;

use super::version::VersionId;
use super::CatalogError;
use crate::access::as_catalog_owner;
use crate::read_only::read_one;
use crate::source::IdType;
use warren_core::graph::NodeOrder;

/// The query [`GraphDefinition::find`] runs, planned once per backend.
/// `current_version` is read with the version it names when that is one of
/// the graph's (`v.version_id`), which a restored or edited row need not be
/// (DESIGN.md §5.1).
const FIND: &str = "SELECT g.graph_id, g.owner::oid, g.readers::oid[], g.node_query, g.edge_query,
                           g.search_path, g.directed, g.weighted, g.has_relations,
                           g.build_reverse, g.retain_versions, g.current_version, g.id_type::oid,
                           v.version_id, g.store_merged, g.node_order, g.source
                    FROM warren.graphs g
                    LEFT JOIN warren.versions v
                           ON v.version_id = g.current_version AND v.graph_id = g.graph_id
                    WHERE g.name = $1";

/// [`GraphDefinition::lock`]'s lock on the graph's row.
const LOCK: &str = "SELECT FROM warren.graphs WHERE graph_id = $1 FOR UPDATE";

/// [`GraphDefinition::share`]'s lock on the graph's row.
const SHARE: &str = "SELECT FROM warren.graphs WHERE graph_id = $1 FOR SHARE";

/// [`GraphDefinition::share`]'s check once it holds the lock.
const STILL_THERE: &str =
    "SELECT EXISTS (SELECT FROM warren.versions WHERE version_id = $1 AND graph_id = $2)";

/// [`GraphDefinition::lock`]'s re-read once it holds the lock:
/// `current_version` as in [`FIND`], and `node_order`, which a compaction
/// that held the lock may have changed.
const REREAD: &str = "SELECT g.current_version, v.version_id, g.node_order
                      FROM warren.graphs g
                      LEFT JOIN warren.versions v
                             ON v.version_id = g.current_version AND v.graph_id = g.graph_id
                      WHERE g.graph_id = $1";

thread_local! {
    /// The saved plan of [`FIND`].
    static FIND_PLAN: OnceCell<OwnedPreparedStatement> = const { OnceCell::new() };
}

/// Where a graph's versions come from (DESIGN.md §5.1, §6.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GraphSource {
    /// Built by `warren.refresh` from its node and edge queries.
    Query,
    /// Derived by `warren.contract` from versions of another graph.
    Contraction,
}

impl GraphSource {
    /// Its name in `graphs.source`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Contraction => "contraction",
        }
    }
}

/// A graph's definition, as `warren.create_graph` (or `warren.contract`)
/// recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GraphDefinition {
    /// `graphs.graph_id`.
    pub(crate) graph_id: i32,
    /// `graphs.name`.
    pub(crate) name: String,
    /// Where its versions come from.
    pub(crate) source: GraphSource,
    /// The owner role: source queries run as this role (DESIGN.md §7).
    pub(crate) owner: pg_sys::Oid,
    /// Roles whose members may query the graph (DESIGN.md §7).
    pub(crate) readers: Vec<pg_sys::Oid>,
    /// The type of its node ids.
    pub(crate) id_type: IdType,
    /// `SELECT <id> ...` (empty for a contraction).
    pub(crate) node_query: String,
    /// `SELECT <src>, <dst> [, <rel>] [, <weight>] ...` (empty for a
    /// contraction).
    pub(crate) edge_query: String,
    /// The `search_path` the source queries run under: the creator's.
    pub(crate) search_path: String,
    /// Whether edges are directed; undirected edges are stored both ways.
    pub(crate) directed: bool,
    /// Whether the edge query has a weight column.
    pub(crate) weighted: bool,
    /// Whether the edge query has a relation column.
    pub(crate) has_relations: bool,
    /// Whether to build the reverse CSRs.
    pub(crate) build_reverse: bool,
    /// Whether to store the merged CSR of several relations, rather than
    /// merge the relations' rows on the fly (DESIGN.md §5.3).
    pub(crate) store_merged: bool,
    /// Promoted versions kept, the current one included.
    pub(crate) retain_versions: i32,
    /// How dense ids are ordered where they may change: at the first build
    /// and at compaction (DESIGN.md §14 item 22).
    pub(crate) node_order: NodeOrder,
    /// The promoted version builds start from, if any.
    pub(crate) current_version: Option<i64>,
    /// The version `graphs.current_version` names when that is not one of
    /// the graph's versions (then `current_version` is `None`).
    pub(crate) stray_current_version: Option<i64>,
}

/// Graph names beginning with this are reserved for the library's level
/// graphs (DESIGN.md §6.7), which only a contraction without `out_graph`
/// creates, under a name no graph has had.
const RESERVED_PREFIX: &str = "_warren_";

/// Refuses a reserved graph name for a graph a caller names.
pub(crate) fn require_unreserved(name: &str) -> Result<(), CatalogError> {
    if name.starts_with(RESERVED_PREFIX) {
        return Err(CatalogError::ReservedGraphName(name.to_owned()));
    }
    Ok(())
}

/// The name of the level graph whose first version is `version`.
pub(crate) fn level_graph_name(version: VersionId) -> String {
    format!("{RESERVED_PREFIX}level_{version}")
}

/// A graph definition to record: the arguments of `warren.create_graph`,
/// plus the owner and the id type it determined.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NewGraph<'a> {
    pub(crate) name: &'a str,
    pub(crate) source: GraphSource,
    pub(crate) owner: pg_sys::Oid,
    /// The source queries (NULL for a contraction).
    pub(crate) node_query: Option<&'a str>,
    pub(crate) edge_query: Option<&'a str>,
    pub(crate) search_path: &'a str,
    pub(crate) id_type: pg_sys::Oid,
    pub(crate) directed: bool,
    pub(crate) weighted: bool,
    pub(crate) has_relations: bool,
    pub(crate) build_reverse: bool,
    pub(crate) store_merged: bool,
    pub(crate) retain_versions: i32,
    pub(crate) node_order: NodeOrder,
}

impl GraphDefinition {
    /// The definition of graph `name`, as of the statement's snapshot. Its
    /// `current_version` is read afresh on every call, through a saved
    /// plan, so a promotion is seen by the next statement (DESIGN.md §5.5).
    pub(crate) fn find(name: &str) -> Result<Self, CatalogError> {
        as_catalog_owner(|| {
            Spi::connect(|client| {
                let row = FIND_PLAN.with(|plan| {
                    if plan.get().is_none() {
                        let prepared = client.prepare(FIND, &[PgOid::from(pg_sys::TEXTOID)])?;
                        // Set only here, after the check above: cannot fail.
                        let _ = plan.set(prepared.keep());
                    }
                    let plan = plan.get().ok_or(pgrx::spi::Error::InvalidPosition)?;
                    Ok::<_, pgrx::spi::Error>(client.select(plan, Some(1), &[name.into()])?.first())
                })?;
                if row.is_empty() {
                    return Err(CatalogError::GraphNotFound(name.to_owned()));
                }
                let id_type = row
                    .get::<pg_sys::Oid>(13)?
                    .and_then(IdType::from_oid)
                    .ok_or_else(|| CatalogError::UnknownIdType(name.to_owned()))?;
                let source = match row.get::<String>(17)?.as_deref() {
                    Some("query") => GraphSource::Query,
                    Some("contraction") => GraphSource::Contraction,
                    _ => return Err(CatalogError::UnknownSource(name.to_owned())),
                };
                Ok(Self {
                    id_type,
                    source,
                    graph_id: row.get(1)?.unwrap_or_default(),
                    name: name.to_owned(),
                    owner: row.get(2)?.unwrap_or(pg_sys::Oid::INVALID),
                    readers: row.get(3)?.unwrap_or_default(),
                    node_query: row.get(4)?.unwrap_or_default(),
                    edge_query: row.get(5)?.unwrap_or_default(),
                    search_path: row.get(6)?.unwrap_or_default(),
                    directed: row.get(7)?.unwrap_or_default(),
                    weighted: row.get(8)?.unwrap_or_default(),
                    has_relations: row.get(9)?.unwrap_or_default(),
                    build_reverse: row.get(10)?.unwrap_or_default(),
                    retain_versions: row.get(11)?.unwrap_or(1),
                    current_version: row.get(14)?,
                    stray_current_version: stray(row.get(12)?, row.get(14)?),
                    store_merged: row.get(15)?.unwrap_or(true),
                    node_order: parse_node_order(&row.get::<String>(16)?.unwrap_or_default())?,
                })
            })
        })
    }

    /// Locks the graph's row until the end of the transaction, so that
    /// refreshes of one graph run one at a time, and re-reads its current
    /// version and node order, which a refresh or compaction that held the
    /// lock may have changed. A graph dropped since [`find`](Self::find) is
    /// not found.
    pub(crate) fn lock(&mut self) -> Result<(), CatalogError> {
        // A row lock is a write (it assigns a transaction id): only the
        // functions that build or drop versions take it. The lock and the
        // re-read are two statements: when the lock waits for another
        // transaction's change to the row, READ COMMITTED re-checks only
        // the locked row (EvalPlanQual), so a version joined in the same
        // statement would be the one from before the wait. The re-read,
        // as a statement of its own, takes a snapshot after the lock.
        let (stored, current, node_order) = as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                let locked = client.update(LOCK, Some(1), &[self.graph_id.into()])?.len();
                if locked == 0 {
                    return Err(CatalogError::GraphNotFound(self.name.clone()));
                }
                let row = client.update(REREAD, Some(1), &[self.graph_id.into()])?.first();
                Ok((row.get::<i64>(1)?, row.get::<i64>(2)?, row.get::<String>(3)?))
            })
        })?;
        self.node_order = parse_node_order(&node_order.unwrap_or_default())?;
        self.current_version = current;
        self.stray_current_version = stray(stored, current);
        Ok(())
    }

    /// Locks the graph's row `FOR SHARE` until the end of the transaction,
    /// so that no promotion (and its retention), refresh, compaction or
    /// drop of the graph or its versions runs meanwhile, then checks that
    /// `version` is still one of its versions: a drop that held the lock
    /// first may have deleted it. A derived version reads its parent
    /// under this lock (DESIGN.md §6.6).
    pub(crate) fn share(&self, version: i64) -> Result<(), CatalogError> {
        let found = as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                let locked = client.update(SHARE, Some(1), &[self.graph_id.into()])?.len();
                if locked == 0 {
                    return Err(CatalogError::GraphNotFound(self.name.clone()));
                }
                // A statement of its own, so its snapshot follows the lock.
                Ok(client
                    .update(STILL_THERE, Some(1), &[version.into(), self.graph_id.into()])?
                    .first()
                    .get_one::<bool>()?)
            })
        })?;
        match found {
            Some(true) => Ok(()),
            _ => Err(CatalogError::VersionNotFound { graph: self.name.clone(), version }),
        }
    }

    /// The graph whose versions this contraction's latest linked version
    /// was contracted from, if any is still linked.
    pub(crate) fn parent_graph(&self) -> Result<Option<i32>, CatalogError> {
        as_catalog_owner(|| {
            Ok(read_one::<i32>(
                "SELECT (SELECT p.graph_id FROM warren.versions v
                         JOIN warren.versions p ON p.version_id = v.parent_version
                         WHERE v.graph_id = $1 ORDER BY v.seq DESC LIMIT 1)",
                &[self.graph_id.into()],
            )?)
        })
    }

    /// Checks that the graph is built from its queries: `refresh` and
    /// `compact` (`action`) build no version of a contraction, which only
    /// `warren.contract` makes (DESIGN.md §6.6).
    pub(crate) fn require_query_source(&self, action: &'static str) -> Result<(), CatalogError> {
        match self.source {
            GraphSource::Query => Ok(()),
            GraphSource::Contraction => {
                Err(CatalogError::DerivedByContraction { graph: self.name.clone(), action })
            }
        }
    }

    /// Checks that the recorded current version, if any, is one of the
    /// graph's versions. Functions that read or build on it call this;
    /// `promote`, the remedy, does not.
    pub(crate) fn require_sound_current_version(&self) -> Result<(), CatalogError> {
        match self.stray_current_version {
            Some(version) => {
                Err(CatalogError::StrayCurrentVersion { graph: self.name.clone(), version })
            }
            None => Ok(()),
        }
    }

    /// Records `order` as the graph's node order (DESIGN.md §14 item 22).
    /// Its row must be locked ([`lock`](Self::lock)).
    /// Renames the graph to `name`, which no graph may have already.
    pub(crate) fn rename(&mut self, name: String) -> Result<(), CatalogError> {
        as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                let taken = client
                    .update(
                        "SELECT 1 FROM warren.graphs WHERE name = $1",
                        Some(1),
                        &[name.as_str().into()],
                    )?
                    .len();
                if taken > 0 {
                    return Err(CatalogError::GraphExists(name.clone()));
                }
                client.update(
                    "UPDATE warren.graphs SET name = $2 WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into(), name.as_str().into()],
                )?;
                Ok(())
            })
        })?;
        self.name = name;
        Ok(())
    }

    pub(crate) fn set_node_order(&mut self, order: NodeOrder) -> Result<(), CatalogError> {
        as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "UPDATE warren.graphs SET node_order = $2 WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into(), order.name().into()],
                )?;
                Ok::<_, CatalogError>(())
            })
        })?;
        self.node_order = order;
        Ok(())
    }

    /// Deletes the graph, and with it every version, segment and vector.
    pub(crate) fn drop_graph(&self) -> Result<(), CatalogError> {
        as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "DELETE FROM warren.graphs WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into()],
                )?;
                Ok(())
            })
        })
    }

    /// Adds `role` to the readers (if `add`) or removes it, pruning reader
    /// roles that no longer exist.
    pub(crate) fn set_reader(&self, role: pg_sys::Oid, add: bool) -> Result<(), CatalogError> {
        as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "UPDATE warren.graphs SET readers = ARRAY(
                         SELECT DISTINCT r.oid FROM pg_catalog.pg_roles r
                         WHERE r.oid = ANY (readers::oid[] || CASE WHEN $3 THEN $2 END)
                           AND r.oid <> CASE WHEN $3 THEN 0::oid ELSE $2 END
                         ORDER BY r.oid)::regrole[]
                     WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into(), role.into(), add.into()],
                )?;
                Ok(())
            })
        })
    }

    /// Makes `owner` the graph's owner, and removes it from the readers,
    /// whose order is kept (DESIGN.md §5.7). Its row must be locked
    /// ([`lock`](Self::lock)).
    pub(crate) fn set_owner(&mut self, owner: pg_sys::Oid) -> Result<(), CatalogError> {
        as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "UPDATE warren.graphs SET owner = $2::oid::regrole, readers = ARRAY(
                         SELECT r.reader FROM pg_catalog.unnest(readers)
                                              WITH ORDINALITY AS r (reader, position)
                         WHERE r.reader::oid <> $2 ORDER BY r.position)::regrole[]
                     WHERE graph_id = $1",
                    None,
                    &[self.graph_id.into(), owner.into()],
                )?;
                Ok::<_, CatalogError>(())
            })
        })?;
        self.owner = owner;
        self.readers.retain(|&reader| reader != owner);
        Ok(())
    }
}

/// The node order named `name` (DESIGN.md §14 item 22), or an
/// `invalid_parameter_value` error.
pub(crate) fn parse_node_order(name: &str) -> Result<NodeOrder, CatalogError> {
    name.parse().map_err(CatalogError::NodeOrder)
}

/// The stored current version when it is not one of the graph's versions.
fn stray(stored: Option<i64>, current: Option<i64>) -> Option<i64> {
    stored.filter(|_| current.is_none())
}

impl NewGraph<'_> {
    /// Records a new graph definition.
    pub(crate) fn create(&self) -> Result<(), CatalogError> {
        let graph = self;
        as_catalog_owner(|| {
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
                    "INSERT INTO warren.graphs (name, owner, node_query, edge_query, search_path,
                         id_type, directed, weighted, has_relations, build_reverse,
                         retain_versions, store_merged, node_order, source)
                     VALUES ($1, $2::oid::regrole, $3, $4, $5, $6::oid::regtype, $7, $8, $9,
                             $10, $11, $12, $13, $14)",
                    None,
                    &[
                        graph.name.into(),
                        graph.owner.into(),
                        graph.node_query.into(),
                        graph.edge_query.into(),
                        graph.search_path.into(),
                        graph.id_type.into(),
                        graph.directed.into(),
                        graph.weighted.into(),
                        graph.has_relations.into(),
                        graph.build_reverse.into(),
                        graph.retain_versions.into(),
                        graph.store_merged.into(),
                        graph.node_order.name().into(),
                        graph.source.name().into(),
                    ],
                )?;
                Ok(())
            })
        })
    }
}
