"""bench/corpus/performance.py: the §12 matrix as it runs on a corpus
dataset."""

from corpus import performance
from corpus.manifest import Dataset


def dataset(**fields):
    base = dict(name="d", tier="medium", domain="social", ids="integer", directed=False, weighted=False,
                relations=False, nodes=0, edges=0, licence="", why="", pin=None)
    return Dataset(**{**base, **fields})


def names(stages):
    return [s.name for s in stages]


def test_the_matrix_covers_the_section_12_rows():
    stages = performance.stages(dataset(), "42::bigint")
    assert {"cold load", "bfs depth 6", "bfs depth 6 recursive CTE", "reachable_set", "components",
            "pagerank 20 iterations", "eigen k=20", "aggregate_neighbours d=64"} <= set(names(stages))


def test_text_ids_are_read_with_a_text_witness():
    stages = {s.name: s for s in performance.stages(dataset(ids="text"), "'Gene::1'::text")}
    assert "NULL::text" in stages["components"].sql
    assert "ARRAY['Gene::1'::text]" in stages["bfs depth 6"].sql


def test_the_recursive_cte_follows_out_edges_of_a_directed_graph_and_both_ways_otherwise():
    directed = performance.cte_sql(dataset(directed=True), "1::bigint")
    undirected = performance.cte_sql(dataset(), "1::bigint")
    assert "UNION ALL SELECT src FROM edges WHERE dst = r.id" not in directed
    assert "UNION ALL SELECT src FROM edges WHERE dst = r.id" in undirected


def test_short_reads_are_timed_in_the_server_and_repeated():
    stages = {s.name: s for s in performance.stages(dataset(), "1::bigint")}
    assert stages["bfs depth 6"].server and stages["bfs depth 6"].repeat == 3
    assert not stages["eigen k=20"].server


def test_throughput_is_in_graph_edges_a_second():
    assert performance.edges_per_second(100_000_000, 4.0) == 25_000_000


def test_a_directed_graph_starts_from_the_node_with_most_out_edges():
    # The busiest node of a directed graph may have no out-edges (a hub
    # that is only cited), so BFS from it would reach nothing.
    assert "SELECT src FROM edges" in performance.start_sql(dataset(directed=True))
    assert "UNION ALL SELECT dst FROM edges" not in performance.start_sql(dataset(directed=True))
    assert "UNION ALL SELECT dst FROM edges" in performance.start_sql(dataset())


def test_the_rows_bfs_reaches_are_counted_beside_the_ctes():
    counted = {s.name: s for s in performance.stages(dataset(), "1::bigint")}
    assert counted["bfs depth 6 rows"].sql == "SELECT count(*) FROM warren.bfs('g', ARRAY[1::bigint], 6)"
    assert counted["reachable_set rows"].sql == "SELECT count(*) FROM warren.reachable_set('g', ARRAY[1::bigint])"
