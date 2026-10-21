//! Neighbourhood similarity for link prediction (DESIGN.md §6.7, planned
//! library group 2): `similarity` scores candidate pairs a query gives
//! (NetworkX's `ebunch`), `most_similar` ranks one node's candidates and
//! `similar_pairs` each node's, by `jaccard`, `adamic_adar` or
//! `common_neighbours` over the undirected simple graph under `rels`.
//! Each returns rows of external ids, computed on the compute pool within
//! the call's memory budget, and persists nothing.

use std::mem::size_of;
use std::sync::Arc;

use pgrx::prelude::*;
use pgrx::spi::SpiTupleTable;
use pgrx::AnyElement;
use warren_core::compute::{self, ComputeError, ComputeMemory, Mask, Neighbourhood, Similarity};
use warren_core::graph::DenseId;

use super::from_query::{check_id_column, id_type_name};
use super::settings::workers;
use super::vectors::Vectors;
use super::{entry, selection, ComputeSqlError};
use crate::access::{with_read_only, with_search_path, Caller};
use crate::cache::{cached, compute_memory, resolve, LoadedVersion};
use crate::external_ids::{check_type, element, external};
use crate::interrupts::PostgresInterrupts;
use crate::selection::relations;
use crate::source::{describe, for_each_tuple, open_read_only, with_id_type, IdDatum, SourceError};

/// The similarity `measure` names.
fn measure(measure: Option<&str>) -> Result<Similarity, ComputeSqlError> {
    Ok(measure.ok_or(ComputeSqlError::NullArgument("measure"))?.parse::<Similarity>()?)
}

/// How many to rank: `k`, which must be at least 1.
fn rank_k(k: Option<i64>) -> Result<u64, ComputeSqlError> {
    match k.ok_or(ComputeSqlError::NullArgument("k"))? {
        k if k < 1 => {
            Err(ComputeSqlError::InvalidArgument(format!("k must be at least 1, not {k}")))
        }
        k => Ok(k as u64),
    }
}

/// `warren.similarity` (§6.7): the `measure` of each pair `pairs` returns
/// (a query of two id columns, run as the caller, read-only), in the
/// query's order; a pair with a NULL, unknown or tombstoned id is skipped.
/// `id_type` fixes the id columns' type, as `warren.components`'s does.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.similarity(graph text, pairs text, id_type anyelement,
                                  measure text DEFAULT 'jaccard', rels text[] DEFAULT NULL,
                                  version bigint DEFAULT NULL)
RETURNS TABLE (source anyelement, target anyelement, score float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'similarity_wrapper';
"#
)]
fn similarity(
    graph: Option<&str>,
    pairs: Option<&str>,
    _id_type: Option<AnyElement>,
    measure_name: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(source, AnyElement), name!(target, AnyElement), name!(score, f64))>
{
    entry(|caller| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let pairs = pairs.ok_or(ComputeSqlError::NullArgument("pairs"))?;
        let measure = measure(measure_name)?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = (AnyElement, AnyElement, f64)>> = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 2 exists.
            let witness = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 2) };
            check_type::<E>(graph, "id_type", witness, false)?;
            let loaded = cached::<E>(&resolved)?;
            let memory = compute_memory();
            let (asked, _charge) = read_pairs(caller, pairs, &loaded, &memory)?;
            let selected = relations(&loaded.relations, rels.as_deref());
            let neighbourhood =
                Neighbourhood {
                    graph: &loaded.graph,
                    relations: selection(selected.as_deref()),
                    tombstones: loaded.id_map.tombstones(),
                };
            let scores = compute::similarity_of_pairs(
                &*workers()?,
                &memory,
                &mut PostgresInterrupts,
                &neighbourhood,
                measure,
                &asked,
            )?;
            Box::new(asked.into_iter().zip(scores.values).filter_map(move |((u, v), score)| {
                Some((external(&loaded.id_map, u)?, external(&loaded.id_map, v)?, score))
            }))
        });
        Ok(TableIterator::new(rows))
    })
}

/// The pairs of live nodes `query`'s rows name, in its order, and their
/// charge.
fn read_pairs<E: IdDatum>(
    caller: &Caller,
    query: &str,
    loaded: &LoadedVersion<E>,
    memory: &ComputeMemory,
) -> Result<(Vec<(DenseId, DenseId)>, compute::MemoryCharge), ComputeSqlError> {
    let query = query.trim_end().trim_end_matches(';');
    with_search_path(caller.search_path(), || {
        with_read_only(|| {
            Spi::connect(|client| {
                let columns = describe::<ComputeSqlError>("pairs", query)?;
                if columns.len() != 2 {
                    return Err(SourceError::ColumnCount {
                        query: "pairs",
                        expected: "2 (source, target)",
                        found: columns.len(),
                    }
                    .into());
                }
                check_id_column::<E>(&columns[0])?;
                check_id_column::<E>(&columns[1])?;
                let id_type = id_type_name::<E>();
                let wrapped =
                    format!("SELECT q.u::{id_type}, q.v::{id_type} FROM ({query}\n) AS q(u, v)");
                let mut opened = open_read_only::<ComputeSqlError>(client, "pairs", &wrapped)?;
                let mut pairs = Pairs { pairs: Vec::new(), charge: memory.charge(0)?, memory };
                for_each_tuple(&mut opened.cursor, |row| pairs.push(loaded, row))?;
                Ok((pairs.pairs, pairs.charge))
            })
        })
    })
}

/// The pairs read so far, their capacity charged.
struct Pairs<'m> {
    pairs: Vec<(DenseId, DenseId)>,
    charge: compute::MemoryCharge,
    memory: &'m ComputeMemory,
}

impl Pairs<'_> {
    /// Adds the row's pair if both ids are live nodes.
    fn push<E: IdDatum>(
        &mut self,
        loaded: &LoadedVersion<E>,
        row: &SpiTupleTable<'_>,
    ) -> Result<(), ComputeSqlError> {
        let dense = |ordinal| -> Result<Option<DenseId>, ComputeSqlError> {
            let id = row.get::<E::Sql>(ordinal)?.map(E::from_sql);
            Ok(id.and_then(|id| loaded.id_map.dense_id(&id)))
        };
        let (Some(u), Some(v)) = (dense(1)?, dense(2)?) else {
            return Ok(());
        };
        if self.pairs.len() == self.pairs.capacity() {
            let capacity = (self.pairs.capacity() * 2).max(1024);
            let bytes = (capacity * size_of::<(DenseId, DenseId)>()) as u64;
            // The larger buffer's charge, then the smaller's release.
            self.charge = self.memory.charge(bytes)?;
            self.pairs
                .try_reserve_exact(capacity - self.pairs.len())
                .map_err(|_| ComputeError::OutOfMemory { bytes })?;
        }
        self.pairs.push((u, v));
        Ok(())
    }
}

/// `warren.most_similar` (§6.7): the `k` nodes most similar to `node` by
/// `measure`, best first (ties by dense id): nodes sharing a neighbour
/// with it that are neither it nor its neighbours (NetworkX's non-edges),
/// in the undirected simple graph under `rels`. Neighbourhoods are read
/// node by node when the graph has reverse CSRs. No rows if `node` is not
/// a live node.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.most_similar(graph text, node anyelement, k bigint DEFAULT 10,
                                    measure text DEFAULT 'jaccard', rels text[] DEFAULT NULL,
                                    version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, score float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'most_similar_wrapper';
"#
)]
fn most_similar(
    graph: Option<&str>,
    node: Option<AnyElement>,
    k: Option<i64>,
    measure_name: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    version: Option<i64>,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(score, f64))> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let node = node.ok_or(ComputeSqlError::NullArgument("node"))?;
        let k = rank_k(k)?;
        let measure = measure(measure_name)?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Vec<(AnyElement, f64)> = with_id_type!(resolved.graph.id_type, E => {
            let id = element::<E>(graph, "node", node)?;
            let loaded = cached::<E>(&resolved)?;
            match loaded.id_map.dense_id(&id) {
                None => Vec::new(),
                Some(dense) => {
                    let selected = relations(&loaded.relations, rels.as_deref());
                    let neighbourhood =
                        Neighbourhood {
                    graph: &loaded.graph,
                    relations: selection(selected.as_deref()),
                    tombstones: loaded.id_map.tombstones(),
                };
                    let similar = compute::most_similar(
                        &*workers()?,
                        &compute_memory(),
                        &mut PostgresInterrupts,
                        &neighbourhood,
                        measure,
                        dense,
                        k,
                    )?;
                    similar
                        .into_iter()
                        .filter_map(|s| Some((external(&loaded.id_map, s.node)?, s.score)))
                        .collect()
                }
            }
        });
        Ok(TableIterator::new(rows))
    })
}

/// `warren.similar_pairs` (§6.7): each node's `k` most similar nodes, as
/// `most_similar` ranks them, ascending by node (dense id) and best first;
/// only for the nodes vector `nodes` selects, if given. Reads the
/// symmetric adjacency, built once per call. `id_type` fixes the id
/// columns' type.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.similar_pairs(graph text, id_type anyelement, k bigint DEFAULT 10,
                                     measure text DEFAULT 'jaccard', nodes text DEFAULT NULL,
                                     rels text[] DEFAULT NULL, version bigint DEFAULT NULL)
RETURNS TABLE (source anyelement, target anyelement, score float8)
STABLE PARALLEL RESTRICTED LANGUAGE c AS 'MODULE_PATHNAME', 'similar_pairs_wrapper';
"#
)]
#[allow(clippy::too_many_arguments)] // The SQL function's arguments.
fn similar_pairs(
    graph: Option<&str>,
    _id_type: Option<AnyElement>,
    k: Option<i64>,
    measure_name: Option<&str>,
    nodes: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(source, AnyElement), name!(target, AnyElement), name!(score, f64))>
{
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let k = rank_k(k)?;
        let measure = measure(measure_name)?;
        let resolved = resolve(graph, version, "read")?;
        let rows: Box<dyn Iterator<Item = (AnyElement, AnyElement, f64)>> = with_id_type!(resolved.graph.id_type, E => {
            // SAFETY: fcinfo is this call's, and argument 1 exists.
            let witness = unsafe { pgrx::fcinfo::pg_getarg_type(fcinfo, 1) };
            check_type::<E>(graph, "id_type", witness, false)?;
            let mut vectors = Vectors::<E>::open(resolved)?;
            let selected_nodes = nodes.map(|name| vectors.get(name)).transpose()?;
            let loaded = Arc::clone(&vectors.loaded);
            let selected = relations(&loaded.relations, rels.as_deref());
            let neighbourhood =
                Neighbourhood {
                    graph: &loaded.graph,
                    relations: selection(selected.as_deref()),
                    tombstones: loaded.id_map.tombstones(),
                };
            let mask = selected_nodes.as_deref().map(|vector| Mask { vector, complement: false });
            let found = compute::similar_pairs(
                &*workers()?,
                &vectors.memory,
                &mut PostgresInterrupts,
                &neighbourhood,
                measure,
                k,
                mask,
            )?;
            let pairs = found.tasks.into_iter().flatten();
            let id_map = Arc::clone(&loaded);
            Box::new(pairs.filter_map(move |(u, similar)| {
                let source = external(&id_map.id_map, u)?;
                Some((source, external(&id_map.id_map, similar.node)?, similar.score))
            }))
        });
        Ok(TableIterator::new(rows))
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, rows, small};

    /// Graph `t` (`small`) and `n`, the same without reverse CSRs: the
    /// undirected simple graph of both is the square 1–2–3–4–1, with 5
    /// (a self-loop) and 6 alone.
    fn squares() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.create_graph('n', 'SELECT id FROM t_nodes', 'SELECT src, dst, rel, w FROM t_edges',
                                        weighted := true, has_relations := true, build_reverse := false);
             SELECT warren.refresh('n')",
        )
    }

    const PAIRS: &str =
        "SELECT * FROM (VALUES (1, 3), (2, 4), (1, 2), (5, 6), (1, 99), (NULL, 2)) v";

    #[pg_test]
    fn pairs_are_scored_in_the_query_s_order_skipping_unknown_ids() -> Result<(), Error> {
        squares()?;
        for graph in ["t", "n"] {
            let scored = |measure: &str| {
                rows(&format!(
                    "SELECT source || '>' || target || ':' || score
                     FROM warren.similarity('{graph}', '{PAIRS}', NULL::bigint, '{measure}')"
                ))
            };
            assert_eq!(scored("jaccard")?.as_deref(), Some("1>3:1 2>4:1 1>2:0 5>6:0"));
            assert_eq!(scored("common_neighbours")?.as_deref(), Some("1>3:2 2>4:2 1>2:0 5>6:0"));
            let adamic_adar = "1>3:2.8853900817779268 2>4:2.8853900817779268 1>2:0 5>6:0";
            assert_eq!(scored("adamic_adar")?.as_deref(), Some(adamic_adar));
        }
        Ok(())
    }

    #[pg_test]
    fn the_most_similar_are_non_neighbours_sharing_a_neighbour() -> Result<(), Error> {
        squares()?;
        for graph in ["t", "n"] {
            let one = rows(&format!(
                "SELECT node_id || ':' || score FROM warren.most_similar('{graph}', 1::bigint)"
            ))?;
            assert_eq!(one.as_deref(), Some("3:1"));
            let unknown = rows(&format!(
                "SELECT node_id || ':' || score FROM warren.most_similar('{graph}', 99::bigint)"
            ))?;
            assert_eq!(unknown.as_deref(), Some(""));
            let each = rows(&format!(
                "SELECT source || '>' || target || ':' || score
                 FROM warren.similar_pairs('{graph}', NULL::bigint, k := 1, measure := 'common_neighbours')"
            ))?;
            assert_eq!(each.as_deref(), Some("1>3:2 2>4:2 3>1:2 4>2:2"));
        }
        Spi::run(
            "SELECT warren.vector_from_query('t', 'some', 'SELECT 2, true', dtype := 'bool', persist := false)",
        )?;
        let some = rows(
            "SELECT source || '>' || target FROM warren.similar_pairs('t', NULL::bigint, nodes := 'some')",
        )?;
        assert_eq!(some.as_deref(), Some("2>4"));
        Ok(())
    }

    #[pg_test]
    fn the_similarities_refuse_bad_arguments() -> Result<(), Error> {
        squares()?;
        for (call, refused) in [
            ("warren.similarity(NULL, 'SELECT 1, 2', NULL::bigint)", "22004: graph must not be NULL"),
            ("warren.similarity('t', NULL, NULL::bigint)", "22004: pairs must not be NULL"),
            (
                "warren.similarity('t', 'SELECT 1, 2', NULL::bigint, 'cosine')",
                "22023: unknown similarity \"cosine\": expected jaccard, adamic_adar or common_neighbours",
            ),
            ("warren.similarity('t', 'SELECT 1, 2', NULL::bigint, NULL)", "22004: measure must not be NULL"),
            ("warren.similarity('t', 'SELECT 1', NULL::bigint)", "42804: pairs must return 2 (source, target) columns, not 1"),
            ("warren.similarity('t', 'SELECT 1.5, 2', NULL::bigint)", "42804: query column 1"),
            ("warren.similarity('t', 'SELECT 1, 2', NULL::uuid)", "42804: graph \"t\" has node ids of type bigint"),
            ("warren.most_similar('t', NULL::bigint)", "22004: node must not be NULL"),
            ("warren.most_similar('t', 1::bigint, k := 0)", "22023: k must be at least 1, not 0"),
            ("warren.most_similar('t', 1::bigint, k := NULL)", "22004: k must not be NULL"),
            ("warren.most_similar('t', 'x'::text)", "42804: graph \"t\" has node ids of type bigint"),
            ("warren.similar_pairs('t', NULL::bigint, k := -1)", "22023: k must be at least 1, not -1"),
            ("warren.similar_pairs('t', NULL::bigint, nodes := 'none')", "42704: graph \"t\" has no vector"),
        ] {
            let found = failure(call)?.unwrap_or_default();
            assert!(found.starts_with(refused), "{call}: {found}");
        }
        Ok(())
    }
}
