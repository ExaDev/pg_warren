//! The SPI adapter of the source port: the node and edge queries run as
//! read-only cursors under the build's pinned snapshot, as the graph owner
//! (DESIGN.md §5.4 step 1, §7).

use std::borrow::Borrow;

use pgrx::prelude::*;
use warren_core::graph::BuildMemory;

use super::error::BuildError;
use super::source::{EdgeRow, EdgeSink, SourceReader};
use crate::access::{as_graph_owner, with_read_only, with_search_path};
use crate::activity::{self, Units};
use crate::catalog::GraphDefinition;
use crate::source::{
    check_edge_columns, for_each_tuple, open_read_only, require_node_id_type, IdDatum,
};

/// A graph's sources, read through SPI.
pub(crate) struct SpiSource<'g> {
    graph: &'g GraphDefinition,
}

impl<'g> SpiSource<'g> {
    /// The sources of `graph`, read as its owner.
    pub(crate) fn new(graph: &'g GraphDefinition) -> Self {
        Self { graph }
    }
}

impl<E: IdDatum> SourceReader<E> for SpiSource<'_> {
    fn node_ids(&mut self, memory: &BuildMemory) -> Result<E::Ids, BuildError> {
        let graph = self.graph;
        as_graph_owner(graph.owner, &graph.name, || {
            with_search_path(&graph.search_path, || {
                with_read_only(|| {
                    Spi::connect(|client| {
                        let mut opened =
                            open_read_only::<BuildError>(client, "node_query", &graph.node_query)?;
                        require_node_id_type(&opened.columns, graph.id_type)?;
                        activity::phase("reading nodes", None);
                        let mut rows = Units::new();
                        let mut ids = E::node_ids(memory);
                        for_each_tuple::<BuildError>(&mut opened.cursor, |row| {
                            rows.one();
                            if let Some(id) = row.id::<E>(1)? {
                                E::push_node_id(&mut ids, id.borrow())?;
                            }
                            Ok(())
                        })?;
                        rows.finish();
                        Ok(ids)
                    })
                })
            })
        })
    }

    fn edges(&mut self, sink: &mut dyn EdgeSink<E::Key>) -> Result<(), BuildError> {
        let graph = self.graph;
        let weight_column = if graph.has_relations { 4 } else { 3 };
        as_graph_owner(graph.owner, &graph.name, || {
            with_search_path(&graph.search_path, || {
                with_read_only(|| {
                    Spi::connect(|client| {
                        let mut opened =
                            open_read_only::<BuildError>(client, "edge_query", &graph.edge_query)?;
                        check_edge_columns(
                            &opened.columns,
                            graph.id_type,
                            graph.has_relations,
                            graph.weighted,
                        )?;
                        sink.expect_rows(opened.estimated_rows);
                        activity::phase("reading edges", Some(opened.estimated_rows));
                        let mut rows = Units::new();
                        for_each_tuple(&mut opened.cursor, |row| {
                            rows.one();
                            let source = row.id::<E>(1)?;
                            let target = row.id::<E>(2)?;
                            sink.row(EdgeRow {
                                source: source.as_ref().map(Borrow::borrow),
                                target: target.as_ref().map(Borrow::borrow),
                                relation: match graph.has_relations {
                                    true => row.text(3)?,
                                    false => None,
                                },
                                weight: match graph.weighted {
                                    true => row.get::<f64>(weight_column)?,
                                    false => None,
                                },
                            })
                        })?;
                        rows.finish();
                        Ok(())
                    })
                })
            })
        })
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use warren_core::graph::FixedKeys;

    use super::SpiSource;
    use crate::build::error::BuildError;
    use crate::build::source::{EdgeRow, EdgeSink, SourceReader};
    use crate::catalog::GraphDefinition;
    use crate::fixture::{allocated, failure, failure_as, ring};
    use crate::source::with_pinned_snapshot;

    /// An edge sink that records the most Postgres memory allocated since
    /// it was made, over the first batch of rows and over all of them.
    struct Probe {
        before: usize,
        first_batch: usize,
        peak: usize,
        rows: u64,
    }

    impl EdgeSink<i64> for Probe {
        fn expect_rows(&mut self, _rows: u64) {}

        fn row(&mut self, row: EdgeRow<'_, i64>) -> Result<(), BuildError> {
            self.peak = self.peak.max(allocated().saturating_sub(self.before));
            self.rows += u64::from(row.relation.is_some());
            if self.rows == 10_000 {
                self.first_batch = self.peak;
            }
            Ok(())
        }
    }

    /// Reading edge rows holds no Postgres memory per row: each relation
    /// name is read in place, not copied into a memory context that lives
    /// until the refresh ends (DESIGN.md §5.4; 100M rows held 1.7 GB): it
    /// stays what one batch of rows takes.
    #[pg_test]
    fn reading_edges_holds_no_memory_per_row() -> Result<(), Error> {
        Spi::run(
            "CREATE ROLE leak_owner;
             CREATE TABLE leak_nodes (id bigint);
             CREATE TABLE leak_edges (src bigint, dst bigint, rel text);
             INSERT INTO leak_nodes SELECT generate_series(1, 1000);
             INSERT INTO leak_edges
                 SELECT g % 1000 + 1, g % 997 + 1, 'relation ' || g % 5
                 FROM generate_series(1, 400000) g;
             GRANT SELECT ON leak_nodes, leak_edges TO leak_owner;
             SET ROLE leak_owner;
             SELECT warren.create_graph('leak', 'SELECT id FROM leak_nodes',
                                        'SELECT src, dst, rel FROM leak_edges',
                                        has_relations := true);
             RESET ROLE",
        )?;
        let graph = GraphDefinition::find("leak").map_err(|_| Error::InvalidPosition)?;
        let mut probe = Probe { before: allocated(), first_batch: 0, peak: 0, rows: 0 };
        with_pinned_snapshot(|| {
            SourceReader::<FixedKeys<i64>>::edges(&mut SpiSource::new(&graph), &mut probe)
        })
        .map_err(|_| Error::InvalidPosition)?;
        assert_eq!(probe.rows, 400_000);
        assert!(
            probe.peak <= probe.first_batch + (256 << 10),
            "{} bytes allocated while reading, {} after the first batch",
            probe.peak,
            probe.first_batch
        );
        Ok(())
    }

    /// Why a non-superuser may not refresh graph `su` (DESIGN.md §7, §14
    /// item 51).
    const REFUSED: &str = "42501: only a superuser may run the source queries of graph \"su\" \
                           as superuser \"sr_super\"";

    /// The remedy the refusal hints at.
    const HINT: &str = "A superuser must do it in its own session (not through SET ROLE or a \
                        SECURITY DEFINER function), or give the graph to a role that is not a \
                        superuser with warren.set_owner or warren.reassign_owned.";

    /// Graph `sr`, a ring of 4 owned by `sr_owner`, and graph `su` over the
    /// same tables, owned by superuser `sr_super`, of which `sr_member` (not
    /// a superuser) is a member and `sr_reader` a reader; `sr_nobody` owns
    /// nothing.
    fn graphs() -> Result<(), Error> {
        ring("sr", 4, 2)?;
        Spi::run(
            "CREATE ROLE sr_super SUPERUSER; CREATE ROLE sr_member; GRANT sr_super TO sr_member;
             CREATE ROLE sr_reader; CREATE ROLE sr_nobody;
             SET ROLE sr_super;
             SELECT warren.create_graph('su', 'SELECT id FROM sr_nodes',
                                        'SELECT src, dst FROM sr_edges');
             RESET ROLE;
             SELECT warren.grant('su', 'sr_reader')",
        )
    }

    #[pg_test]
    fn a_superuser_refreshes_a_graph_a_superuser_owns() -> Result<(), Error> {
        graphs()?;
        assert_eq!(failure("warren.refresh('su')")?, None);
        assert_eq!(failure_as("sr_super", "warren.refresh('su')")?, None);
        Ok(())
    }

    /// Superuser is not inherited: a member of a superuser role is not a
    /// superuser, and may not run SQL as one through a graph.
    #[pg_test]
    fn only_a_superuser_refreshes_a_graph_a_superuser_owns() -> Result<(), Error> {
        graphs()?;
        assert_eq!(failure_as("sr_member", "warren.refresh('su')")?.as_deref(), Some(REFUSED));
        for role in ["sr_reader", "sr_nobody", "sr_owner"] {
            let refused = failure_as(role, "warren.refresh('su')")?.unwrap_or_default();
            assert!(refused.starts_with("42501: "), "{role}: {refused}");
        }
        assert_eq!(failure_as("sr_owner", "warren.refresh('sr')")?, None);
        Ok(())
    }

    /// A holder of `pg_write_all_data` (a restore, DESIGN.md §14 item 51)
    /// can rewrite a graph's owner and source queries in `warren.graphs`:
    /// the queries must not then run as a superuser for a non-superuser.
    #[pg_test]
    fn a_row_rewritten_to_a_superuser_owner_is_refused_before_its_queries_run() -> Result<(), Error>
    {
        graphs()?;
        Spi::run(
            "UPDATE warren.graphs SET owner = 'sr_super'::regrole, node_query = 'SELECT 1 / 0'
             WHERE name = 'sr'",
        )?;
        let refused = REFUSED.replace("\"su\"", "\"sr\"");
        assert_eq!(failure_as("sr_member", "warren.refresh('sr')")?, Some(refused));
        assert_eq!(
            failure("warren.refresh('sr')")?.as_deref(),
            Some("22012: division by zero"),
            "a superuser runs what the row holds"
        );
        Ok(())
    }

    /// The session user must be a superuser too: a superuser's `SECURITY
    /// DEFINER` function does not let a non-superuser's session through.
    #[pg_test]
    fn a_superusers_security_definer_function_does_not_refresh_for_others() -> Result<(), Error> {
        graphs()?;
        Spi::run(
            "CREATE FUNCTION sr_refresh() RETURNS bigint SECURITY DEFINER LANGUAGE sql
                 AS $$ SELECT warren.refresh('su') $$;
             CREATE TEMP TABLE refusal (what text, hint text); GRANT ALL ON refusal TO PUBLIC;
             SET SESSION AUTHORIZATION sr_nobody;
             DO $do$ DECLARE hint text; BEGIN PERFORM sr_refresh();
             EXCEPTION WHEN OTHERS THEN
                 GET STACKED DIAGNOSTICS hint = PG_EXCEPTION_HINT;
                 INSERT INTO refusal VALUES (SQLSTATE || ': ' || SQLERRM, hint);
             END $do$;
             RESET SESSION AUTHORIZATION",
        )?;
        let refused = Spi::get_one::<String>("SELECT max(what) FROM refusal")?;
        assert_eq!(refused.as_deref(), Some(REFUSED));
        let hint = Spi::get_one::<String>("SELECT max(hint) FROM refusal")?;
        assert_eq!(hint.as_deref(), Some(HINT));
        Ok(())
    }
}
