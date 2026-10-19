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
