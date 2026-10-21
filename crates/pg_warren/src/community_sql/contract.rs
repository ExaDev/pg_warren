//! `warren.contract` (DESIGN.md §6.6): a new version of a quotient graph,
//! one node per distinct label of a label vector of the parent version,
//! the edges between communities aggregated, and the membership stored
//! with it. The quotient graph is created by the first contraction into
//! it, owned by the caller; later contractions into it add versions.

use pgrx::prelude::*;
use warren_core::community::Contraction;

use super::contraction::{contract_version, ContractRequest};
use super::CommunitySqlError;
use crate::access::{current_user, entry, require_owner, Caller};
use crate::build::{max_build_memory, RefreshRequest, SpiCatalog};
use crate::cache::resolve;
use crate::catalog::{CatalogError, GraphDefinition, GraphSource, Lineage, NewGraph};
use crate::compute_sql::{workers, Vectors};
use crate::interrupts::PostgresInterrupts;
use crate::segment::{SpiSegmentStore, CHUNK_SIZE};
use crate::source::{with_id_type, IdType};
use warren_core::graph::NodeOrder;

/// `warren.contract`: contracts version `version` (NULL: the current one)
/// of `graph` by the labels of its vector `label_vector` into a new
/// version of graph `out_graph`, created if it does not exist, and returns
/// the new version's id. Each live node belongs to the community of its
/// label, which must be an integer; the quotient's node ids (`bigint`)
/// are the labels. Edges between communities are summed by weight (or
/// counted if not `weighted`); edges inside a community make its
/// self-loop, unless `drop_self_loops`. The caller must be a reader of
/// `graph`, and a member of `out_graph`'s owner role if it exists. The
/// version is made current if `promote`, and is otherwise left staged.
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn contract(
    graph: Option<&str>,
    label_vector: Option<&str>,
    out_graph: Option<&str>,
    weighted: default!(Option<bool>, "true"),
    drop_self_loops: default!(Option<bool>, "false"),
    promote: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|caller| {
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        let label_vector = label_vector.ok_or(CommunitySqlError::NullArgument("label_vector"))?;
        let out_graph = out_graph.ok_or(CommunitySqlError::NullArgument("out_graph"))?;
        let weighted = weighted.ok_or(CommunitySqlError::NullArgument("weighted"))?;
        let drop_self_loops =
            drop_self_loops.ok_or(CommunitySqlError::NullArgument("drop_self_loops"))?;
        let promote = promote.ok_or(CommunitySqlError::NullArgument("promote"))?;
        if graph == out_graph {
            return Err(CommunitySqlError::InvalidArgument(format!(
                "out_graph must not be graph \"{graph}\" itself"
            )));
        }
        let parent = resolve(graph, version, "contract")?;
        parent.graph.share(parent.version)?;
        let built = with_id_type!(parent.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(parent)?;
            let labels = vectors.get(label_vector)?;
            let lineage = Lineage {
                parent_version: vectors.version,
                label_vector: vectors.persisted_id(label_vector),
            };
            let mut quotient = quotient_graph(out_graph, &vectors.graph, caller)?;
            quotient.lock()?;
            quotient.require_sound_current_version()?;
            let request = ContractRequest {
                write: RefreshRequest {
                    promote,
                    label: None,
                    chunk_size: CHUNK_SIZE,
                    memory_budget: max_build_memory(),
                },
                contraction: Contraction {
                    weighted,
                    drop_self_loops,
                    undirected: !vectors.graph.directed,
                },
                lineage,
            };
            contract_version(
                &quotient,
                &vectors.loaded,
                &labels,
                &request,
                (&*workers()?, &mut PostgresInterrupts),
                &mut SpiSegmentStore,
                &mut SpiCatalog::at_current_wal_lsn()?,
            )?
        });
        Ok(Some(built))
    })
}

/// Graph `name`, which a contraction of `parent` adds a version to: an
/// existing graph derived by contraction, from graphs of the same shape
/// (direction and relations), whose owner role the caller is a member of;
/// or a new one, owned by the caller.
fn quotient_graph(
    name: &str,
    parent: &GraphDefinition,
    caller: &Caller,
) -> Result<GraphDefinition, CommunitySqlError> {
    let existing = match GraphDefinition::find(name) {
        Ok(existing) => existing,
        Err(CatalogError::GraphNotFound(_)) => {
            NewGraph {
                name,
                source: GraphSource::Contraction,
                owner: current_user(),
                node_query: None,
                edge_query: None,
                search_path: caller.search_path(),
                id_type: IdType::Int8.oid(),
                directed: parent.directed,
                weighted: true,
                has_relations: parent.has_relations,
                build_reverse: parent.build_reverse,
                store_merged: parent.store_merged,
                retain_versions: 2,
                node_order: NodeOrder::Id,
            }
            .create()?;
            return Ok(GraphDefinition::find(name)?);
        }
        Err(error) => return Err(error.into()),
    };
    require_owner(existing.owner, &existing.name, "contract into")?;
    if existing.source != GraphSource::Contraction {
        return Err(CommunitySqlError::NotAContraction(existing.name));
    }
    // Versions from another parent would carry its topology into this
    // graph's readers' view and mix two label spaces.
    if existing.parent_graph()?.is_some_and(|graph| graph != parent.graph_id) {
        return Err(CommunitySqlError::InvalidArgument(format!(
            "graph \"{}\" is contracted from another graph than \"{}\": contract into a new graph",
            existing.name, parent.name
        )));
    }
    if (existing.directed, existing.has_relations) != (parent.directed, parent.has_relations) {
        return Err(CommunitySqlError::InvalidArgument(format!(
            "graph \"{}\" was created by contracting a {} graph{}, but graph \"{}\" is {}{}",
            existing.name,
            direction(existing.directed),
            relations(existing.has_relations),
            parent.name,
            direction(parent.directed),
            relations(parent.has_relations),
        )));
    }
    Ok(existing)
}

fn direction(directed: bool) -> &'static str {
    if directed {
        "directed"
    } else {
        "undirected"
    }
}

fn relations(has_relations: bool) -> &'static str {
    if has_relations {
        " with relations"
    } else {
        " without relations"
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{current, failure, refresh, rows, small, versions};

    /// Graph `t` (`fixture::small`) with label vector `l`: nodes 1 and 2
    /// in community 10, 3 and 4 in 20, 5 and 6 in 30.
    fn labelled() -> Result<i64, Error> {
        let version = small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'l',
                 'SELECT id, CASE WHEN id <= 2 THEN 10 WHEN id <= 4 THEN 20 ELSE 30 END
                  FROM t_nodes')",
        )?;
        Ok(version)
    }

    fn contract(call: &str) -> Result<i64, Error> {
        Spi::get_one::<i64>(&format!("SELECT {call}"))?.ok_or(Error::InvalidPosition)
    }

    /// Each node of `graph` with its weighted out-degree.
    fn out_weights(graph: &str) -> Result<Option<String>, Error> {
        Spi::run(&format!(
            "SELECT warren.vector_const('{graph}', 'one', 1);
             SELECT warren.spmv('{graph}', 'one', 'out')"
        ))?;
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('{graph}', 'out', NULL::bigint)"
        ))
    }

    #[pg_test]
    fn contract_creates_a_quotient_graph_and_records_its_lineage() -> Result<(), Error> {
        let parent = labelled()?;
        let version = contract("warren.contract('t', 'l', 'q')")?;
        assert_eq!(current("q")?, Some(version));
        let info = Spi::get_one::<String>(
            "SELECT format('%s|%s|%s|%s|%s|%s', i ->> 'source', i ->> 'id_type',
                           i #>> '{versions,0,parent_version}', i #>> '{versions,0,parent_graph}',
                           (i #>> '{versions,0,label_vector}')::bigint
                               = (SELECT vector_id FROM warren.vectors WHERE name = 'l'),
                           i ->> 'owner' = current_user)
             FROM warren.info('q') i",
        )?;
        assert_eq!(info, Some(format!("contraction|bigint|{parent}|t|t|t")));
        let ids = rows("SELECT node_id || ':' || live FROM warren.id_map('q', NULL::bigint)")?;
        assert_eq!(ids.as_deref(), Some("10:true 20:true 30:true"));
        Ok(())
    }

    #[pg_test]
    fn edges_between_communities_are_summed_or_counted() -> Result<(), Error> {
        labelled()?;
        Spi::run(
            "SELECT warren.contract('t', 'l', 'q');
             SELECT warren.contract('t', 'l', 'counted', weighted := false);
             SELECT warren.contract('t', 'l', 'between', drop_self_loops := true)",
        )?;
        // 10 → 10: 1; 10 → 20: 1 + 5; 20 → 20: 1; 20 → 10: 1; 30 → 30: 2.
        assert_eq!(out_weights("q")?.as_deref(), Some("10:7 20:2 30:2"));
        assert_eq!(out_weights("counted")?.as_deref(), Some("10:3 20:2 30:1"));
        assert_eq!(out_weights("between")?.as_deref(), Some("10:6 20:1 30:0"));
        Ok(())
    }

    #[pg_test]
    fn an_undirected_edge_inside_a_community_counts_both_ways() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE u_nodes (id bigint); CREATE TABLE u_edges (src bigint, dst bigint);
             INSERT INTO u_nodes VALUES (1), (2), (3);
             INSERT INTO u_edges VALUES (1, 2), (2, 3), (3, 3);
             SELECT warren.create_graph('u', 'SELECT id FROM u_nodes',
                                        'SELECT src, dst FROM u_edges', directed := false);
             SELECT warren.refresh('u');
             SELECT warren.vector_from_query('u', 'l',
                 'SELECT id, CASE WHEN id <= 2 THEN 0 ELSE 1 END FROM u_nodes');
             SELECT warren.contract('u', 'l', 'uq', weighted := false)",
        )?;
        // 0: 1 – 2 both ways, and 2 – 3; 1: 3 – 2, and the self-loop once.
        assert_eq!(out_weights("uq")?.as_deref(), Some("0:3 1:2"));
        let directed = Spi::get_one::<bool>("SELECT (warren.info('uq') ->> 'directed')::bool")?;
        assert_eq!(directed, Some(false));
        Ok(())
    }

    /// The quotient keeps weighted degree, as pg_warren counts it (a
    /// self-loop once): a community's is the sum of its members'.
    #[pg_test]
    fn a_communitys_weighted_degree_is_its_members() -> Result<(), Error> {
        Spi::run(
            "CREATE TABLE w_nodes AS SELECT generate_series(1, 40)::bigint AS id;
             CREATE TABLE w_edges AS
             SELECT (g % 40 + 1)::bigint AS src, ((g * 7) % 40 + 1)::bigint AS dst, (1 + g % 5)::float8 AS w
             FROM generate_series(1, 150) g;
             SELECT warren.create_graph('w', 'SELECT id FROM w_nodes', 'SELECT src, dst, w FROM w_edges',
                                        directed := false, weighted := true);
             SELECT warren.refresh('w');
             SELECT warren.vector_from_query('w', 'l', 'SELECT id, id % 6 FROM w_nodes');
             SELECT warren.contract('w', 'l', 'wq')",
        )?;
        out_weights("w")?;
        Spi::run("SELECT warren.project('w', 'out', 'wq', 'member_degrees')")?;
        out_weights("wq")?;
        let differ = Spi::get_one::<i64>(
            "SELECT count(*) FROM warren.vector_values('wq', 'out', NULL::bigint) q
             JOIN warren.vector_values('wq', 'member_degrees', NULL::bigint) p USING (node_id)
             WHERE abs(q.value - p.value) > 1e-9",
        )?;
        assert_eq!(differ, Some(0));
        Ok(())
    }

    #[pg_test]
    fn a_contraction_into_an_existing_quotient_adds_a_version_with_stable_ids() -> Result<(), Error>
    {
        labelled()?;
        let first = contract("warren.contract('t', 'l', 'q')")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'l',
                 'SELECT id, CASE WHEN id <= 2 THEN 10 ELSE 40 END FROM t_nodes')",
        )?;
        let staged = contract("warren.contract('t', 'l', 'q', promote := false)")?;
        assert_eq!(versions("q")?, Some(format!("{first}:false {staged}:true")));
        let ids = rows(&format!(
            "SELECT node_id || ':' || dense_id || ':' || live
             FROM warren.id_map('q', NULL::bigint, version := {staged})"
        ))?;
        assert_eq!(ids.as_deref(), Some("10:0:true 20:1:false 30:2:false 40:3:true"));
        Ok(())
    }

    #[pg_test]
    fn a_quotients_versions_come_only_from_contract() -> Result<(), Error> {
        labelled()?;
        Spi::run("SELECT warren.contract('t', 'l', 'q')")?;
        assert_eq!(
            failure("warren.refresh('q')")?.as_deref(),
            Some(
                "42809: cannot refresh graph \"q\": it is derived by contraction; \
                 warren.contract makes its versions"
            )
        );
        assert_eq!(
            failure("warren.compact('q')")?.as_deref(),
            Some(
                "42809: cannot compact graph \"q\": it is derived by contraction; \
                 warren.contract makes its versions"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn a_quotient_takes_versions_from_one_parent_graph_only() -> Result<(), Error> {
        labelled()?;
        small("secret")?;
        Spi::run(
            "SELECT warren.vector_const('secret', 'l', 1, 'i64');
             SELECT warren.contract('t', 'l', 'q')",
        )?;
        assert_eq!(
            failure("warren.contract('secret', 'l', 'q')")?.as_deref(),
            Some(
                "22023: graph \"q\" is contracted from another graph than \"secret\": \
                 contract into a new graph"
            )
        );
        Ok(())
    }

    #[pg_test]
    fn contract_refuses_a_graph_built_from_queries_or_itself() -> Result<(), Error> {
        labelled()?;
        small("other")?;
        assert_eq!(
            failure("warren.contract('t', 'l', 'other')")?.as_deref(),
            Some(
                "42809: graph \"other\" is built from queries, not derived by contraction: \
                 contract into a new graph, or into one warren.contract created"
            )
        );
        assert_eq!(
            failure("warren.contract('t', 'l', 't')")?.as_deref(),
            Some("22023: out_graph must not be graph \"t\" itself")
        );
        Ok(())
    }

    #[pg_test]
    fn labels_must_be_integers() -> Result<(), Error> {
        labelled()?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'half', 'SELECT id, id / 2.0 FROM t_nodes');
             SELECT warren.vector_from_query('t', 'nan', 'SELECT id, ''NaN''::float8 FROM t_nodes');
             SELECT warren.vector_const('t', 'flag', 1, 'bool')",
        )?;
        assert_eq!(
            failure("warren.contract('t', 'half', 'q')")?.as_deref(),
            Some("22023: label vector: the label of dense id 0 is 0.5, which is not an integer")
        );
        assert_eq!(
            failure("warren.contract('t', 'nan', 'q')")?.as_deref(),
            Some("22023: label vector: the label of dense id 0 is NaN, which is not an integer")
        );
        assert_eq!(
            failure("warren.contract('t', 'flag', 'q')")?.as_deref(),
            Some("42804: label vector: a label vector must be numeric (i64, f64 or f32), not bool")
        );
        assert_eq!(
            failure("warren.contract('t', 'none', 'q')")?.as_deref(),
            Some(format!(
                "42704: graph \"t\" has no vector \"none\" at version {}",
                current("t")?.unwrap_or_default()
            ))
            .as_deref()
        );
        let graphs = Spi::get_one::<i64>("SELECT count(*) FROM warren.graphs WHERE name = 'q'")?;
        assert_eq!(graphs, Some(0), "a failed contraction creates no graph");
        Ok(())
    }

    #[pg_test]
    fn a_reader_may_contract_into_a_graph_of_its_own_only() -> Result<(), Error> {
        labelled()?;
        Spi::run(
            "CREATE ROLE t_reader; CREATE ROLE t_stranger;
             SELECT warren.grant('t', 't_reader');
             SELECT warren.contract('t', 'l', 'q')",
        )?;
        Spi::run("SET ROLE t_reader")?;
        let own = Spi::get_one::<i64>("SELECT warren.contract('t', 'l', 'mine')");
        let others = failure("warren.contract('t', 'l', 'q')");
        Spi::run("RESET ROLE")?;
        assert!(own?.is_some());
        let owner = Spi::get_one::<String>("SELECT warren.info('mine') ->> 'owner'")?;
        assert_eq!(owner.as_deref(), Some("t_reader"));
        let superuser = Spi::get_one::<String>("SELECT current_user::text")?.unwrap_or_default();
        assert_eq!(
            others?,
            Some(format!(
                "42501: must be a member of role \"{superuser}\" to contract into graph \"q\""
            ))
        );
        Spi::run("DROP TABLE failure; SET ROLE t_stranger")?;
        let stranger = failure("warren.contract('t', 'l', 'theirs')");
        Spi::run("RESET ROLE")?;
        assert_eq!(
            stranger?.as_deref(),
            Some(
                "42501: must be a member of role \"t_owner\" or of a reader role to contract \
                 graph \"t\""
            )
        );
        Ok(())
    }

    #[pg_test]
    fn the_parent_version_is_kept_while_a_quotient_version_derives_from_it() -> Result<(), Error> {
        let parent = labelled()?;
        let quotient = contract("warren.contract('t', 'l', 'q')")?;
        // retain_versions is 2: two more promoted versions retire the first,
        // but the quotient's version keeps it.
        refresh("t", true)?;
        refresh("t", true)?;
        let kept = Spi::get_one::<bool>(&format!(
            "SELECT EXISTS (SELECT FROM warren.versions WHERE version_id = {parent})"
        ))?;
        assert_eq!(kept, Some(true));
        // Dropping it anyway unlinks the quotient's version.
        Spi::run(&format!("SELECT warren.drop_version('t', {parent})"))?;
        let link = Spi::get_one::<i64>(&format!(
            "SELECT parent_version FROM warren.versions WHERE version_id = {quotient}"
        ))?;
        assert_eq!(link, None);
        Ok(())
    }

    #[pg_test]
    fn dropping_the_parent_graph_or_the_label_vector_leaves_no_dangling_link() -> Result<(), Error>
    {
        labelled()?;
        let quotient = contract("warren.contract('t', 'l', 'q')")?;
        Spi::run("SELECT warren.drop_vector('t', 'l')")?;
        let link = |column: &str| {
            Spi::get_one::<i64>(&format!(
                "SELECT {column} FROM warren.versions WHERE version_id = {quotient}"
            ))
        };
        assert_eq!(link("label_vector")?, None);
        assert!(link("parent_version")?.is_some());
        Spi::run("SELECT warren.drop_graph('t')")?;
        assert_eq!(link("parent_version")?, None);
        assert_eq!(current("q")?, Some(quotient), "the quotient graph remains");
        Ok(())
    }

    #[pg_test]
    fn a_transient_label_vector_is_not_recorded() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_dense_id('t', 'd', persist := false);
             SELECT warren.contract('t', 'd', 'q')",
        )?;
        let label =
            Spi::get_one::<String>("SELECT warren.info('q') #>> '{versions,0,label_vector}'")?;
        assert_eq!(label, None);
        let nodes =
            Spi::get_one::<String>("SELECT warren.info('q') #>> '{versions,0,node_count}'")?;
        assert_eq!(nodes.as_deref(), Some("6"));
        Ok(())
    }

    #[pg_test]
    fn a_quotient_can_be_contracted_again() -> Result<(), Error> {
        labelled()?;
        Spi::run(
            "SELECT warren.contract('t', 'l', 'q');
             SELECT warren.vector_from_query('q', 'top', 'SELECT id, 1 FROM (VALUES (10), (20), (30)) v(id)');
             SELECT warren.contract('q', 'top', 'qq')",
        )?;
        assert_eq!(out_weights("qq")?.as_deref(), Some("1:11"));
        Ok(())
    }
}
