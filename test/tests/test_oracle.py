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
