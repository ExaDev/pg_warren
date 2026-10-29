//! `warren.pagerank` (DESIGN.md §6.7): PageRank as NetworkX computes it,
//! one `warren_core::compute::pagerank` run on the compute pool within the
//! call's memory budget, its output persisted. Through 0.7.0 it was a
//! PL/pgSQL function over the primitives; it keeps that function's
//! signature, results to the bit, SQLSTATEs and progress reports.

use pgrx::prelude::*;
use warren_core::compute::{self, AnyVector, PageRank};

use super::super::vectors::{refuse_reserved, Vectors};
use super::super::{entry, selection, ComputeSqlError};
use crate::cache::resolve;
use crate::interrupts::{workers, CommandInterrupts};
use crate::selection::relations;
use crate::source::with_id_type;

/// `warren.pagerank` (§6.7): vector `out` (`f64`, summing to 1) holding
/// each live node's PageRank over the relations `rels` (all if NULL),
/// weighted if `weighted` (NULL: if the graph has weights), a pair linked
/// in several relations one edge or with `per_relation` an edge in each
/// (§5.3): rank flows along edges, split by out-weight; dangling nodes'
/// rank and the `1 − damping` teleport go to the personalisation, uniform
/// or proportional to vector `seed`. It stops when an iteration's L1
/// change falls below live nodes × `tol`, or after `max_iter` iterations,
/// and returns the iterations run. As the PL/pgSQL library did, a NULL
/// `tol` never stops it early and a NULL `max_iter` runs none. Reports
/// command `pagerank`, phase `iterating`, of `max_iter` (§5.7).
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.pagerank(graph text, "out" text, damping float8 DEFAULT 0.85,
                                tol float8 DEFAULT 1e-6, max_iter int DEFAULT 100,
                                seed text DEFAULT NULL, rels text[] DEFAULT NULL,
                                weighted boolean DEFAULT NULL, version bigint DEFAULT NULL,
                                per_relation boolean DEFAULT false)
RETURNS int
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'pagerank_wrapper';
"#
)]
#[allow(clippy::too_many_arguments)] // The SQL function's arguments.
fn pagerank(
    graph: Option<&str>,
    out: Option<&str>,
    damping: Option<f64>,
    tol: Option<f64>,
    max_iter: Option<i32>,
    seed: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    weighted: Option<bool>,
    version: Option<i64>,
    per_relation: Option<bool>,
) -> Option<i32> {
    entry(|_| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let out = out.ok_or(ComputeSqlError::NullArgument("out"))?;
        let damping = damping.ok_or(ComputeSqlError::NullArgument("damping"))?;
        let per_relation = per_relation.ok_or(ComputeSqlError::NullArgument("per_relation"))?;
        let max_iter = max_iter.map_or(0, |max_iter| u32::try_from(max_iter).unwrap_or(0));
        let resolved = resolve(graph, version, "write vectors of")?;
        refuse_reserved(out)?;
        seed.map(refuse_reserved).transpose()?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            vectors.check_writable(out, true)?;
            let seed = seed.map(|name| seeded(&mut vectors, name)).transpose()?;
            let loaded = &vectors.loaded;
            let selected = relations(&loaded.relations, rels.as_deref());
            let pagerank = PageRank {
                relations: selection(selected.as_deref()),
                weighted,
                per_relation,
                damping,
                tol,
                max_iter,
                seed: seed.as_deref(),
            };
            let interrupts = &mut CommandInterrupts::new("pagerank", vectors.graph.graph_id);
            let (memory, tombstones) = (&vectors.memory, vectors.tombstones());
            let ranks =
                compute::pagerank(&*workers()?, memory, interrupts, &loaded.graph, tombstones, &pagerank)?;
            vectors.save(out, ranks.values.into(), true)?;
            Ok(Some(ranks.iterations as i32))
        })
    })
}

/// Vector `name`, the personalisation: refused if `bool`, as no operator
/// multiplies a `boolean` by a `double precision`.
fn seeded<E: crate::cache::CachedIds>(
    vectors: &mut Vectors<E>,
    name: &str,
) -> Result<std::sync::Arc<AnyVector>, ComputeSqlError> {
    let seed = vectors.get(name)?;
    match *seed {
        AnyVector::Bool(_) => Err(ComputeSqlError::BooleanSeed(name.to_owned())),
        _ => Ok(seed),
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, merged_pair, small};

    /// Defines `public.pagerank_reference`, the PL/pgSQL function the
    /// kernel replaced.
    fn reference() -> Result<(), Error> {
        Spi::run(include_str!("pagerank_reference.sql"))
    }

    /// Whether `warren.pagerank(graph, 'pr', <arguments>)` returns what
    /// `pagerank_reference(graph, 'pr_ref', <arguments>)` does and writes
    /// the same ranks, bit for bit (`float8send`), or else the difference.
    fn same_as_reference(graph: &str, arguments: &str) -> Result<Option<String>, Error> {
        let kernel =
            Spi::get_one::<i32>(&format!("SELECT warren.pagerank('{graph}', 'pr'{arguments})"))?;
        let reference = Spi::get_one::<i32>(&format!(
            "SELECT pagerank_reference('{graph}', 'pr_ref'{arguments})"
        ))?;
        if kernel != reference {
            return Ok(Some(format!("{arguments}: {kernel:?} iterations, not {reference:?}")));
        }
        Spi::get_one::<String>(&format!(
            "SELECT string_agg(node_id || ': ' || k.value || ' <> ' || r.value, ', ')
             FROM warren.vector_values('{graph}', 'pr', NULL::bigint) k
             FULL JOIN warren.vector_values('{graph}', 'pr_ref', NULL::bigint) r USING (node_id)
             WHERE float8send(k.value) IS DISTINCT FROM float8send(r.value)"
        ))
        .map(|differs| differs.map(|differs| format!("{arguments}: {differs}")))
    }

    #[pg_test]
    fn ranks_are_the_plpgsql_librarys_to_the_bit() -> Result<(), Error> {
        reference()?;
        merged_pair("m")?;
        Spi::run(
            "CREATE TABLE m_n AS SELECT id FROM m_nodes;
             SELECT warren.create_graph('m_n', 'SELECT id FROM m_n',
                                        'SELECT src, dst, rel, w FROM m_edges',
                                        weighted := true, has_relations := true,
                                        build_reverse := false);
             SELECT warren.refresh('m_n')",
        )?;
        for graph in ["m_s", "m_u", "m_n"] {
            Spi::run(&format!(
                "SELECT warren.vector_from_query('{graph}', 'seed',
                     'SELECT id, (id % 5)::float8 FROM m_nodes WHERE id % 3 = 0');
                 SELECT warren.vector_from_query('{graph}', 'seed_i',
                     'SELECT id, id % 4 FROM m_nodes', 'i64')"
            ))?;
            for arguments in [
                "",
                ", tol := 1e-12, max_iter := 1000",
                ", tol := 0, max_iter := 20",
                ", weighted := false",
                ", per_relation := true",
                ", rels := ARRAY['r1', 'r0']",
                ", rels := ARRAY['r2'], per_relation := true, weighted := false",
                ", seed := 'seed'",
                ", seed := 'seed_i', damping := 0.5",
                ", damping := 1, tol := NULL, max_iter := 9",
                ", max_iter := 0",
                ", tol := 'NaN'",
            ] {
                assert_eq!(same_as_reference(graph, arguments)?, None, "{graph}");
            }
        }
        Ok(())
    }

    /// The PL/pgSQL library's answers to odd arguments, which the kernel
    /// keeps; its NULL `out` failed only once it had run, with `y` named.
    #[pg_test]
    fn odd_arguments_are_answered_as_the_plpgsql_library_answered() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'flags', 'SELECT id, id > 2 FROM t_nodes', 'bool');
             SELECT warren.vector_from_query('t', 'zero', 'SELECT id, 0 FROM t_nodes');
             SELECT warren.vector_from_query('t', 'wide', 'SELECT id, ARRAY[id, 1]::float8[] FROM t_nodes');
             CREATE TABLE e_nodes (id bigint);
             CREATE TABLE e_edges (src bigint, dst bigint);
             SELECT warren.create_graph('e', 'SELECT id FROM e_nodes', 'SELECT src, dst FROM e_edges');
             SELECT warren.refresh('e')",
        )?;
        for (call, answer) in [
            ("warren.pagerank(NULL, 'pr')", "22004: graph must not be NULL"),
            ("warren.pagerank('t', NULL)", "22004: out must not be NULL"),
            ("warren.pagerank('t', 'pr', damping := NULL)", "22004: damping must not be NULL"),
            (
                "warren.pagerank('t', 'pr', per_relation := NULL)",
                "22004: per_relation must not be NULL",
            ),
            ("warren.pagerank('t', '')", "22023: a vector name must not be empty"),
            ("warren.pagerank('nope', 'pr')", "42704: graph \"nope\" does not exist"),
            (
                "warren.pagerank('t', 'pr', seed := 'nope')",
                "42704: graph \"t\" has no vector \"nope\"",
            ),
            (
                "warren.pagerank('t', 'pr', seed := 'flags')",
                "42883: operator does not exist: boolean * double precision",
            ),
            ("warren.pagerank('t', 'pr', seed := 'zero')", "22012: division by zero"),
            (
                "warren.pagerank('t', 'pr', seed := 'wide')",
                "22023: vector width 1 does not match width 2",
            ),
            ("warren.pagerank('e', 'pr', max_iter := 0)", "22012: division by zero"),
            ("warren.pagerank('t', 'pr', tol := 1e308)", "22003: value out of range: overflow"),
        ] {
            let failed = failure(call)?.unwrap_or_default();
            assert!(failed.starts_with(answer), "{call}: {failed}");
        }
        for (call, iterations) in [
            ("warren.pagerank('t', 'pr', tol := NULL, max_iter := 3)", 3),
            ("warren.pagerank('t', 'pr', max_iter := NULL)", 0),
            ("warren.pagerank('t', 'pr', max_iter := -3)", 0),
            ("warren.pagerank('t', 'pr', seed := 'wide', max_iter := 0)", 0),
            ("warren.pagerank('t', 'pr', rels := ARRAY['nope'])", 1),
        ] {
            assert_eq!(Spi::get_one::<i32>(&format!("SELECT {call}"))?, Some(iterations), "{call}");
        }
        Ok(())
    }
}
