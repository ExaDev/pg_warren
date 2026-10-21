"""NetworkX reference results."""

import networkx as nx
import pytest

from harness import oracle


def graph(edges, nodes=(), directed=False):
    g = nx.DiGraph() if directed else nx.Graph()
    g.add_nodes_from(nodes)
    g.add_edges_from(edges)
    return g


def test_components_label_each_node_with_its_components_smallest_id():
    rows = oracle.compute("components", graph([(5, 2), (2, 9), (4, 7)], nodes=[1]), [])
    assert sorted(rows) == [(1, 1), (2, 2), (4, 4), (5, 2), (7, 4), (9, 2)]


def test_components_of_a_directed_graph_are_weakly_connected():
    rows = oracle.compute("components", graph([(3, 1), (2, 1)], directed=True), [])
    assert sorted(rows) == [(1, 1), (2, 1), (3, 1)]


def test_pagerank_of_a_cycle_is_uniform_and_sums_to_one():
    rows = oracle.compute("pagerank", graph([(1, 2), (2, 3), (3, 1)], directed=True), ["--damping", "0.85"])
    assert [n for n, _ in sorted(rows)] == [1, 2, 3]
    assert all(score == pytest.approx(1 / 3) for _, score in rows)


def test_pagerank_favours_the_node_everyone_links_to():
    rows = dict(oracle.compute("pagerank", graph([(1, 4), (2, 4), (3, 4), (4, 1)], directed=True), []))
    assert max(rows, key=rows.get) == 4
    assert sum(rows.values()) == pytest.approx(1)


def test_the_spectrum_is_a_dense_solvers_over_nodes_with_an_edge():
    # The path 1 – 2 – 3 – 4 plus isolated node 9: L has eigenvalues
    # 2 − 2cos(πj/4); the isolated node is left out, as warren.eigen does.
    path = graph([(1, 2), (2, 3), (3, 4)], nodes=[9])
    rows = oracle.compute("spectrum", path, ["--matrix", "laplacian", "--which", "smallest", "--k", "3"])
    assert [i for i, _ in rows] == [1, 2, 3]
    assert [value for _, value in rows] == pytest.approx([0, 2 - 2 ** 0.5, 2], abs=1e-12)
    largest = oracle.compute("spectrum", path, ["--k", "2"])
    assert [value for _, value in largest] == pytest.approx([1.618033988749895, -1.618033988749895])
    normalized = oracle.compute("spectrum", path, ["--matrix", "normalized_laplacian", "--which", "smallest",
                                                   "--k", "1"])
    assert normalized[0][1] == pytest.approx(0, abs=1e-12)


def test_an_unknown_computation_is_refused():
    with pytest.raises(ValueError, match="unknown oracle"):
        oracle.compute("diameter", graph([]), [])


def test_each_computation_names_its_result_table_and_columns():
    assert oracle.TABLES["components"] == ("oracle_components", "node_id bigint, component bigint")
    assert oracle.TABLES["core_number"] == ("oracle_core_number", "node_id bigint, core bigint")
    assert oracle.TABLES["dag_longest_path"] == ("oracle_dag_longest_path", "node_id bigint, length float8")
    assert oracle.TABLES["spanning_forest"] == ("oracle_spanning_forest", "component bigint, weight float8")
    assert oracle.TABLES["strong_components"] == ("oracle_strong_components", "node_id bigint, component bigint")
    assert oracle.TABLES["topological_order"] == ("oracle_topological_order", "node_id bigint, position bigint")
    assert oracle.TABLES["pagerank"] == ("oracle_pagerank", "node_id bigint, score float8")
    assert oracle.TABLES["spectrum"] == ("oracle_spectrum", "i int, eigenvalue float8")
    assert oracle.TABLES["triangles"] == ("oracle_triangles", "node_id bigint, triangles bigint")


def test_triangles_count_each_nodes_triangles_ignoring_direction_and_self_loops():
    rows = oracle.compute("triangles", graph([(1, 2), (2, 3), (3, 1), (1, 3), (3, 4), (4, 4)], nodes=[5],
                                             directed=True), [])
    assert sorted(rows) == [(1, 1), (2, 1), (3, 1), (4, 0), (5, 0)]


class Rows:
    """`database.query` over fixed node and edge rows, for `oracle.export`."""

    def __init__(self, nodes, edges):
        self.nodes, self.edges = nodes, edges

    def query(self, sql):
        return [[str(v) for v in row] for row in (self.nodes if sql == "nodes" else self.edges)]


def test_a_third_edge_column_is_the_weight():
    exported = oracle.export(Rows([(1,), (2,)], [(1, 2, 2.5)]), "nodes", "edges", directed=True)
    assert exported[1][2]["weight"] == 2.5
    unweighted = oracle.export(Rows([(1,), (2,)], [(1, 2)]), "nodes", "edges", directed=True)
    assert "weight" not in unweighted[1][2]


def test_eigenvector_centrality_is_networkxs_with_weights():
    g = graph([(1, 2), (2, 3), (3, 1), (3, 4)])
    g[3][4]["weight"] = 3.0
    rows = dict(oracle.compute("eigenvector", g, []))
    assert rows == pytest.approx(nx.eigenvector_centrality(g, weight="weight"))
    assert rows[3] > rows[1]


def test_katz_centrality_takes_alpha_beta_and_normalized():
    g = graph([(1, 2), (2, 3), (3, 1), (4, 1)], directed=True)
    rows = dict(oracle.compute("katz", g, ["--alpha", "0.2", "--beta", "0.5", "--no-normalized"]))
    assert rows == pytest.approx(nx.katz_centrality(g, alpha=0.2, beta=0.5, normalized=False, weight="weight"))
    assert rows[4] == pytest.approx(0.5)
    normalized = dict(oracle.compute("katz", g, []))
    assert sum(v * v for v in normalized.values()) == pytest.approx(1)


def test_hits_gives_each_node_a_hub_and_an_authority_score():
    g = graph([(1, 3), (2, 3), (1, 4)], directed=True)
    rows = {node: (hub, authority) for node, hub, authority in oracle.compute("hits", g, [])}
    assert max(rows, key=lambda n: rows[n][0]) == 1
    assert max(rows, key=lambda n: rows[n][1]) == 3
    assert sum(hub for hub, _ in rows.values()) == pytest.approx(1)
    assert sum(authority for _, authority in rows.values()) == pytest.approx(1)


def test_majority_label_propagation_takes_the_most_frequent_label_smallest_first():
    star = graph([(5, 1), (5, 2), (5, 3), (5, 4)])
    rows = dict(oracle.compute("label_propagation_majority", star, []))
    # Step 1: leaves see {self, 5} and take min(self, 5); the hub sees five
    # labels once each and takes 1. Step 2: leaves see {self, 1}: 1.
    assert rows == {1: 1, 2: 1, 3: 1, 4: 1, 5: 1}
    rows = dict(oracle.compute("label_propagation_majority", star, ["--max-iter", "1"]))
    assert rows == {1: 1, 2: 2, 3: 3, 4: 4, 5: 1}


def test_majority_label_propagation_ignores_direction():
    rows = dict(oracle.compute("label_propagation_majority", graph([(2, 1)], directed=True), []))
    assert rows == {1: 1, 2: 1}


def test_clustering_is_networkxs_on_the_undirected_simple_graph():
    g = graph([(1, 2), (2, 3), (3, 1), (1, 4), (4, 1), (4, 4)], directed=True)
    rows = dict(oracle.compute("clustering", g, []))
    assert rows == pytest.approx({1: 1 / 3, 2: 1.0, 3: 1.0, 4: 0.0})


def test_a_pair_given_twice_is_one_edge_of_their_summed_weight():
    exported = oracle.export(Rows([(1,), (2,)], [(1, 2, 2.5), (1, 2, 1.0), (2, 1, 4.0)]), "nodes", "edges",
                             directed=True)
    assert (exported[1][2]["weight"], exported[2][1]["weight"]) == (3.5, 4.0)
    undirected = oracle.export(Rows([(1,), (2,)], [(1, 2, 2.5), (2, 1, 4.0)]), "nodes", "edges", directed=False)
    assert undirected[1][2]["weight"] == 6.5


def weighted(edges, nodes=(), directed=False):
    g = nx.DiGraph() if directed else nx.Graph()
    g.add_nodes_from(nodes)
    for source, target, weight in edges:
        oracle.add_edge(g, source, target, weight)
    return g


def test_core_numbers_ignore_direction_and_self_loops():
    # A 4-clique on 1..4 with a tail 4 → 5 → 6, a self-loop on 6, and 7.
    clique = [(a, b) for a in range(1, 5) for b in range(1, 5) if a < b]
    rows = oracle.compute("core_number", graph(clique + [(4, 5), (5, 6), (6, 6)], nodes=[7], directed=True), [])
    assert sorted(rows) == [(1, 3), (2, 3), (3, 3), (4, 3), (5, 1), (6, 1), (7, 0)]


def test_strong_components_are_labelled_by_their_smallest_node():
    rows = oracle.compute("strong_components", graph([(3, 1), (1, 4), (4, 3), (4, 2), (5, 5)], nodes=[6],
                                                     directed=True), [])
    assert sorted(rows) == [(1, 1), (2, 2), (3, 1), (4, 1), (5, 5), (6, 6)]


def test_the_topological_order_takes_the_smallest_ready_node_first():
    rows = oracle.compute("topological_order", graph([(5, 2), (2, 4), (5, 1)], nodes=[3], directed=True), [])
    assert sorted(rows) == [(1, 2), (2, 3), (3, 0), (4, 4), (5, 1)]


def test_longest_paths_end_at_each_node():
    dag = weighted([(1, 2, 2.0), (2, 4, 1.0), (1, 3, 1.0), (3, 4, 5.0), (4, 5, 4.0)], nodes=[6], directed=True)
    rows = oracle.compute("dag_longest_path", dag, [])
    assert sorted(rows) == [(1, 0), (2, 2), (3, 1), (4, 6), (5, 10), (6, 0)]
    hops = oracle.compute("dag_longest_path", graph([(1, 2), (2, 3), (1, 3)], directed=True), [])
    assert sorted(hops) == [(1, 0), (2, 1), (3, 2)]


def test_the_spanning_forest_weighs_each_component_and_keeps_the_lighter_direction():
    # Square 1 – 2 – 3 – 4 – 1 (1, 4, 2, 3) with diagonal 1 – 3 (5); 5 → 6
    # (9) and 6 → 5 (7); 7 alone.
    edges = [(1, 2, 1.0), (3, 2, 4.0), (3, 4, 2.0), (4, 1, 3.0), (1, 3, 5.0), (5, 6, 9.0), (6, 5, 7.0)]
    rows = oracle.compute("spanning_forest", weighted(edges, nodes=[7], directed=True), [])
    assert sorted(rows) == [(1, 6.0), (5, 7.0), (7, 0.0)]


def test_longest_paths_restart_after_a_negative_prefix():
    # 1 → 2 (−3) → 3 (2); 1 → 3 (1): into 3 the best is 1 → 3 (1), not
    # 1 → 2 → 3 (−1), and into 2 the empty path (0) beats −3.
    dag = weighted([(1, 2, -3.0), (2, 3, 2.0), (1, 3, 1.0), (3, 4, -1.0)], directed=True)
    rows = oracle.compute("dag_longest_path", dag, [])
    assert sorted(rows) == [(1, 0), (2, 0), (3, 2), (4, 1)]


def test_a_quotient_sums_the_edges_between_and_inside_each_label():
    # Labels 0: {1, 2}, 1: {3}; edges 1 – 2 (inside 0), 2 – 3, 1 – 3, 3 – 3.
    g = graph([(1, 2), (2, 3), (1, 3), (3, 3)])
    for u, v in g.edges:
        g[u][v]["weight"] = u + v
    labels = {1: 0, 2: 0, 3: 1}
    rows = sorted(oracle.quotient(g, [], labels))
    assert rows == [(0, 0, 3.0), (0, 1, 9.0), (1, 0, 9.0), (1, 1, 6.0)]
    counted = sorted(oracle.quotient(g, ["--count"], labels))
    assert counted == [(0, 0, 1.0), (0, 1, 2.0), (1, 0, 2.0), (1, 1, 1.0)]


def test_a_directed_quotient_keeps_the_direction_of_each_edge():
    g = graph([(1, 3), (3, 2), (2, 1)], directed=True)
    rows = sorted(oracle.quotient(g, ["--count"], {1: 0, 2: 0, 3: 1}))
    assert rows == [(0, 0, 1.0), (0, 1, 1.0), (1, 0, 1.0)]


def test_louvain_finds_two_cliques_and_scores_them():
    g = graph([(a, b) for a in range(4) for b in range(a + 1, 4)]
              + [(a, b) for a in range(4, 8) for b in range(a + 1, 8)] + [(3, 4)])
    rows = sorted(oracle.compute("louvain", g, []))
    assert [(node, community) for node, community, _ in rows] == [(n, 0 if n < 4 else 4) for n in range(8)]
    assert rows[0][2] == pytest.approx(2 * (6 / 13 - (13 / 26) ** 2))
