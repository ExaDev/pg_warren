//! `warren.project` and `warren.lift` (DESIGN.md §6.6): vectors moved
//! between a parent version and a quotient version contracted from it,
//! through the quotient version's stored membership. The quotient version
//! names the parent version, so the two always correspond.

use pgrx::prelude::*;
use warren_core::community::{self, Aggregate};
use warren_core::compute::{ComputeMemory, MemoryCharge};
use warren_core::graph::Membership;

use super::CommunitySqlError;
use crate::access::{entry, is_reader};
use crate::cache::{cached, compute_memory, resolve, Resolved};
use crate::catalog::{parent_of, GraphDefinition, GraphSource};
use crate::compute_sql::Vectors;
use crate::segment::{read_segment, SpiSegmentStore, MEMBERSHIP};
use crate::source::{with_id_type, IdDatum};

/// `warren.project`: vector `y` of `out_graph`'s version `version` (NULL:
/// its current one) as each community's `agg` (`sum`, `mean`, `max`, `min`
/// or `count`) of vector `x` over its members, read at the version of
/// `graph` that `out_graph`'s version was contracted from: `Pᵀ·x`. Works
/// row by row on matrices. `y` is `double precision` (`bigint` for
/// `count`), persisted unless `persist` is false. The caller must be a
/// reader of both graphs.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn project(
    graph: Option<&str>,
    x: Option<&str>,
    out_graph: Option<&str>,
    y: Option<&str>,
    agg: default!(Option<&str>, "'sum'"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) {
    entry(|_| -> Result<(), CommunitySqlError> {
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        let x = x.ok_or(CommunitySqlError::NullArgument("x"))?;
        let out_graph = out_graph.ok_or(CommunitySqlError::NullArgument("out_graph"))?;
        let y = y.ok_or(CommunitySqlError::NullArgument("y"))?;
        let aggregate: Aggregate = agg.ok_or(CommunitySqlError::NullArgument("agg"))?.parse()?;
        let persist = persist.ok_or(CommunitySqlError::NullArgument("persist"))?;
        let pair = Pair::resolve(graph, out_graph, version)?;
        with_id_type!(pair.parent.graph.id_type, E => {
            let mut opened = pair.open::<E>()?;
            let vector = opened.parent.get(x)?;
            let projected =
                community::project(&opened.memory, &opened.membership, &vector, aggregate)?;
            opened.quotient.save(y, projected, persist)?;
            Ok(())
        })
    })
}

/// `warren.lift`: vector `x` of the version of `graph` that `out_graph`'s
/// version `version` (NULL: its current one) was contracted from, each
/// node holding its community's row of `out_graph`'s vector `y`: `P·y`.
/// `x` has `y`'s element type and width, and is persisted unless
/// `persist` is false. The caller must be a reader of both graphs.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn lift(
    out_graph: Option<&str>,
    y: Option<&str>,
    graph: Option<&str>,
    x: Option<&str>,
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) {
    entry(|_| -> Result<(), CommunitySqlError> {
        let out_graph = out_graph.ok_or(CommunitySqlError::NullArgument("out_graph"))?;
        let y = y.ok_or(CommunitySqlError::NullArgument("y"))?;
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        let x = x.ok_or(CommunitySqlError::NullArgument("x"))?;
        let persist = persist.ok_or(CommunitySqlError::NullArgument("persist"))?;
        let pair = Pair::resolve(graph, out_graph, version)?;
        with_id_type!(pair.parent.graph.id_type, E => {
            let mut opened = pair.open::<E>()?;
            let vector = opened.quotient.get(y)?;
            let lifted = community::lift(&opened.memory, &opened.membership, &vector)?;
            opened.parent.save(x, lifted, persist)?;
            Ok(())
        })
    })
}

/// A quotient version and the parent version it was contracted from.
struct Pair {
    parent: Resolved,
    quotient: Resolved,
}

/// A [`Pair`]'s versions and vectors, with the membership linking them,
/// within one compute budget.
struct Opened<E> {
    parent: Vectors<E>,
    quotient: Vectors<i64>,
    membership: Membership,
    _membership_charge: MemoryCharge,
    memory: ComputeMemory,
}

impl Pair {
    /// `out_graph`'s version `version` (NULL: the current one) and the
    /// version of `graph` it was contracted from. The caller must be a
    /// reader of both.
    fn resolve(
        graph: &str,
        out_graph: &str,
        version: Option<i64>,
    ) -> Result<Self, CommunitySqlError> {
        let quotient = resolve(out_graph, version, "read")?;
        if quotient.graph.source != GraphSource::Contraction {
            return Err(CommunitySqlError::NotDerived {
                graph: quotient.graph.name,
                version: quotient.version,
            });
        }
        let Some(parent) = parent_of(quotient.version)? else {
            return Err(CommunitySqlError::ParentDropped {
                graph: quotient.graph.name,
                version: quotient.version,
            });
        };
        let named = GraphDefinition::find(graph)?;
        if named.graph_id != parent.graph_id {
            // Name the parent only to a caller who may read it.
            let actual = GraphDefinition::find(&parent.graph)?;
            if !is_reader(actual.owner, &actual.readers)? {
                return Err(CommunitySqlError::NotContractedFrom {
                    quotient: quotient.graph.name,
                    version: quotient.version,
                    graph: named.name,
                });
            }
            return Err(CommunitySqlError::OtherParent {
                quotient: quotient.graph.name,
                version: quotient.version,
                parent_version: parent.version,
                parent: parent.graph,
                graph: named.name,
            });
        }
        let parent = resolve(graph, Some(parent.version), "read")?;
        Ok(Self { parent, quotient })
    }

    /// Loads both versions and the membership, charged to one budget.
    fn open<E: IdDatum + Send + Sync>(self) -> Result<Opened<E>, CommunitySqlError> {
        let parent_loaded = cached::<E>(&self.parent)?;
        let quotient_loaded = cached::<i64>(&self.quotient)?;
        let memory = compute_memory();
        let parent_nodes = parent_loaded.id_map.id_space();
        // The segment as read, and the membership decoded from it.
        let reading = memory.charge(parent_nodes.saturating_mul(16).saturating_add(16))?;
        let bytes = read_segment(&mut SpiSegmentStore, self.quotient.version, MEMBERSHIP)?;
        let membership = Membership::decode(&bytes, quotient_loaded.id_map.id_space())?;
        drop((bytes, reading));
        let membership_charge = memory.charge(membership.bytes())?;
        membership.check_parent(parent_loaded.id_map.tombstones())?;
        Ok(Opened {
            parent: Vectors::with_loaded(self.parent, parent_loaded, &memory)?,
            quotient: Vectors::with_loaded(self.quotient, quotient_loaded, &memory)?,
            membership,
            _membership_charge: membership_charge,
            memory,
        })
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, refresh, rows, small};

    /// Graph `t` (`fixture::small`) contracted into `q` by label vector
    /// `l` (nodes 1 and 2 in community 10, 3 and 4 in 20, 5 and 6 in 30),
    /// with vector `x` (each node's id) and matrix `m` (id, 10 × id).
    fn contracted() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'l',
                 'SELECT id, CASE WHEN id <= 2 THEN 10 WHEN id <= 4 THEN 20 ELSE 30 END
                  FROM t_nodes');
             SELECT warren.vector_from_query('t', 'x', 'SELECT id, id FROM t_nodes');
             SELECT warren.vector_from_query('t', 'm',
                 'SELECT id, ARRAY[id, 10 * id]::float8[] FROM t_nodes');
             SELECT warren.contract('t', 'l', 'q')",
        )
    }

    fn values(graph: &str, name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('{graph}', '{name}', NULL::bigint)"
        ))
    }

    #[pg_test]
    fn project_aggregates_a_vector_over_each_community() -> Result<(), Error> {
        contracted()?;
        for agg in ["sum", "mean", "max", "min", "count"] {
            Spi::run(&format!("SELECT warren.project('t', 'x', 'q', '{agg}', '{agg}')"))?;
        }
        assert_eq!(values("q", "sum")?.as_deref(), Some("10:3 20:7 30:11"));
        assert_eq!(values("q", "mean")?.as_deref(), Some("10:1.5 20:3.5 30:5.5"));
        assert_eq!(values("q", "max")?.as_deref(), Some("10:2 20:4 30:6"));
        assert_eq!(values("q", "min")?.as_deref(), Some("10:1 20:3 30:5"));
        assert_eq!(values("q", "count")?.as_deref(), Some("10:2 20:2 30:2"));
        let dtype = Spi::get_one::<String>(
            "SELECT string_agg(name || ':' || dtype, ' ' ORDER BY name) FROM warren.vectors
             WHERE name IN ('sum', 'count')",
        )?;
        assert_eq!(dtype.as_deref(), Some("count:i64 sum:f64"));
        Ok(())
    }

    #[pg_test]
    fn project_takes_the_mean_of_a_matrix_row_by_row() -> Result<(), Error> {
        contracted()?;
        Spi::run("SELECT warren.project('t', 'm', 'q', 'centroid', 'mean', persist := false)")?;
        let found = rows(
            "SELECT node_id || ':' || row::text FROM warren.matrix_rows('q', 'centroid', NULL::bigint)",
        )?;
        assert_eq!(found.as_deref(), Some("10:{1.5,15} 20:{3.5,35} 30:{5.5,55}"));
        Ok(())
    }

    #[pg_test]
    fn lift_broadcasts_each_communitys_value_to_its_members() -> Result<(), Error> {
        contracted()?;
        Spi::run(
            "SELECT warren.project('t', 'x', 'q', 'total');
             SELECT warren.lift('q', 'total', 't', 'community_total')",
        )?;
        assert_eq!(values("t", "community_total")?.as_deref(), Some("1:3 2:3 3:7 4:7 5:11 6:11"));
        Ok(())
    }

    #[pg_test]
    fn a_tombstoned_node_belongs_to_no_community() -> Result<(), Error> {
        contracted()?;
        Spi::run("DELETE FROM t_nodes WHERE id = 6")?;
        refresh("t", true)?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'l', 'SELECT id, 1 FROM t_nodes');
             SELECT warren.vector_const('t', 'one', 1);
             SELECT warren.contract('t', 'l', 'all');
             SELECT warren.project('t', 'one', 'all', 'members');
             SELECT warren.lift('all', 'members', 't', 'back')",
        )?;
        assert_eq!(values("all", "members")?.as_deref(), Some("1:5"));
        let back = Spi::get_one::<String>(
            "SELECT string_agg(dense_id || ':' || live, ' ' ORDER BY dense_id)
             FROM warren.id_map('t', NULL::bigint)",
        )?;
        assert_eq!(back.as_deref(), Some("0:true 1:true 2:true 3:true 4:true 5:false"));
        assert_eq!(values("t", "back")?.as_deref(), Some("1:5 2:5 3:5 4:5 5:5"));
        Ok(())
    }

    #[pg_test]
    fn x_is_read_at_the_version_the_quotient_was_contracted_from() -> Result<(), Error> {
        contracted()?;
        let parent = Spi::get_one::<i64>(
            "SELECT (warren.info('q') #>> '{versions,0,parent_version}')::bigint",
        )?
        .unwrap_or_default();
        refresh("t", true)?;
        Spi::run("SELECT warren.vector_const('t', 'later', 1)")?;
        assert_eq!(
            failure("warren.project('t', 'later', 'q', 'y')")?,
            Some(format!("42704: graph \"t\" has no vector \"later\" at version {parent}"))
        );
        Ok(())
    }

    #[pg_test]
    fn project_and_lift_need_a_quotient_of_that_graph() -> Result<(), Error> {
        contracted()?;
        small("other")?;
        let quotient =
            Spi::get_one::<i64>("SELECT current_version FROM warren.graphs WHERE name = 'q'")?
                .unwrap_or_default();
        let parent = Spi::get_one::<i64>(
            "SELECT (warren.info('q') #>> '{versions,0,parent_version}')::bigint",
        )?
        .unwrap_or_default();
        assert_eq!(
            failure("warren.project('other', 'x', 'q', 'y')")?,
            Some(format!(
                "22023: version {quotient} of graph \"q\" was contracted from version {parent} \
                 of graph \"t\", not of graph \"other\""
            ))
        );
        let other =
            Spi::get_one::<i64>("SELECT current_version FROM warren.graphs WHERE name = 'other'")?
                .unwrap_or_default();
        assert_eq!(
            failure("warren.lift('other', 'x', 't', 'y')")?,
            Some(format!(
                "42809: version {other} of graph \"other\" was not derived by contraction"
            ))
        );
        assert_eq!(
            failure("warren.project('t', 'x', 'q', 'y', 'avg')")?.as_deref(),
            Some("22023: unknown aggregate \"avg\": expected sum, mean, max, min or count")
        );
        Spi::run("SELECT warren.drop_graph('t')")?;
        assert_eq!(
            failure("warren.lift('q', 'y', 't', 'x')")?,
            Some(format!(
                "55000: version {quotient} of graph \"q\" was contracted from a version that has \
                 since been dropped"
            ))
        );
        Ok(())
    }

    /// A quotient's reader who cannot read its parent learns nothing of
    /// it: not from info, not from the error naming the graph it is not
    /// contracted from.
    #[pg_test]
    fn a_parent_the_caller_cannot_read_is_not_named() -> Result<(), Error> {
        contracted()?;
        small("other")?;
        Spi::run(
            "CREATE ROLE q_only; SELECT warren.grant('q', 'q_only'); SELECT warren.grant('other', 'q_only');
             SET ROLE q_only",
        )?;
        let info = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s', i #>> '{versions,0,parent_graph}', i #>> '{versions,0,parent_version}',
                           i #>> '{versions,0,label_vector}')
             FROM warren.info('q') i",
        );
        let refused = failure("warren.project('other', 'x', 'q', 'y')");
        Spi::run("RESET ROLE")?;
        assert_eq!(info?.as_deref(), Some("||"));
        let quotient =
            Spi::get_one::<i64>("SELECT current_version FROM warren.graphs WHERE name = 'q'")?
                .unwrap_or_default();
        assert_eq!(
            refused?,
            Some(format!(
                "22023: version {quotient} of graph \"q\" was not contracted from graph \"other\""
            ))
        );
        let shown =
            Spi::get_one::<String>("SELECT warren.info('q') #>> '{versions,0,parent_graph}'")?;
        assert_eq!(shown.as_deref(), Some("t"), "the parent's reader sees it");
        Ok(())
    }

    #[pg_test]
    fn the_caller_must_read_both_graphs() -> Result<(), Error> {
        contracted()?;
        Spi::run("CREATE ROLE q_reader; SELECT warren.grant('q', 'q_reader'); SET ROLE q_reader")?;
        let refused = failure("warren.project('t', 'x', 'q', 'y')");
        Spi::run("RESET ROLE")?;
        assert_eq!(
            refused?.as_deref(),
            Some("42501: must be a member of role \"t_owner\" or of a reader role to read graph \"t\"")
        );
        Ok(())
    }
}
