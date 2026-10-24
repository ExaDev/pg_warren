"""bench/corpus/oracle.py: NetworkX references read from a dataset's tables,
under pg_warren's definitions (deduplicated edges, neighbours counted once,
the undirected view of DESIGN.md §6.7 for modularity)."""

import csv

import networkx as nx
import pytest

from corpus import oracle
from corpus.manifest import Dataset


def dataset(**fields):
    base = dict(name="d", tier="tiny", domain="social", ids="integer", directed=True, weighted=False,
                relations=False, nodes=0, edges=0, licence="", why="", pin=None)
    return Dataset(**{**base, **fields})


def tables(tmp_path, nodes, edges):
    for name, rows in (("nodes", [[n] for n in nodes]), ("edges", edges)):
        with open(tmp_path / f"{name}.csv", "w", newline="") as f:
            csv.writer(f).writerows(rows)
    return tmp_path


def test_parallel_edges_are_one_edge_and_their_weights_add_up(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2", "3"], [["1", "2", "2"], ["1", "2", "3"], ["2", "3", "1"]]),
                                      dataset(weighted=True))
    assert reference.graph["1"]["2"]["weight"] == 5
    assert reference.graph.number_of_nodes() == 3


def test_degree_counts_each_neighbour_once_a_self_loop_included(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2", "3"], [["1", "2"], ["1", "1"], ["3", "1"]]),
                                      dataset())
    assert reference.degrees("out") == {"1": 2, "2": 0, "3": 1}
    assert reference.degrees("in") == {"1": 2, "2": 1, "3": 0}
    assert reference.degrees("both") == {"1": 3, "2": 1, "3": 1}


def test_edges_in_several_relations_are_one_edge_of_the_graph(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["a", "b"], [["a", "b", "x"], ["a", "b", "y"]]),
                                      dataset(ids="text", relations=True))
    assert reference.graph.number_of_edges() == 1


def test_bfs_depths_follow_out_edges(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2", "3", "4"], [["1", "2"], ["2", "3"], ["4", "1"]]),
                                      dataset())
    assert reference.bfs_depths("1") == {"1": 0, "2": 1, "3": 2}


def test_triangles_clustering_and_cores_are_of_the_undirected_simple_graph(tmp_path):
    reference = oracle.Reference.read(
        tables(tmp_path, ["1", "2", "3", "4"], [["1", "2"], ["2", "3"], ["3", "1"], ["1", "3"], ["3", "3"], ["3", "4"]]),
        dataset())
    assert reference.triangles() == {"1": 1, "2": 1, "3": 1, "4": 0}
    assert reference.core_number() == {"1": 2, "2": 2, "3": 2, "4": 1}
    assert reference.clustering()["3"] == pytest.approx(1 / 3)


def test_the_undirected_view_adds_an_edge_and_its_reverse(tmp_path):
    # B = A + Aᵀ: 1→2 and 2→1 make weight 2; a directed self-loop counts
    # twice in its node's degree, so NetworkX (which counts a self-loop's
    # weight twice) gets its weight unchanged.
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2", "3"], [["1", "2"], ["2", "1"], ["2", "3"], ["3", "3"]]),
                                      dataset())
    view = reference.undirected_view()
    assert (view["1"]["2"]["weight"], view["2"]["3"]["weight"], view["3"]["3"]["weight"]) == (2, 1, 1)


def test_an_undirected_self_loop_counts_once_in_the_view(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2"], [["1", "2"], ["2", "2"]]), dataset(directed=False))
    assert reference.undirected_view()["2"]["2"]["weight"] == 0.5


def test_modularity_of_a_partition_is_of_the_undirected_view(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2", "3", "4"], [["1", "2"], ["3", "4"]]),
                                      dataset(directed=False))
    assert reference.modularity({"1": 0, "2": 0, "3": 1, "4": 1}) == pytest.approx(0.5)


def test_louvain_returns_a_partition_and_its_modularity(tmp_path):
    edges = [[str(a), str(b)] for a, b in nx.barbell_graph(5, 0).edges()]
    reference = oracle.Reference.read(tables(tmp_path, [str(n) for n in range(10)], edges), dataset(directed=False))
    labels, q = reference.louvain()
    assert len(set(labels.values())) == 2
    assert q == pytest.approx(reference.modularity(labels))


def test_the_view_counts_a_pair_once_unless_asked_per_relation(tmp_path):
    # An unweighted pair in two relations weighs 1, as in spmv's A
    # (docs/compute.md), or 2 counted per relation.
    reference = oracle.Reference.read(tables(tmp_path, ["a", "b"], [["a", "b", "x"], ["a", "b", "y"], ["a", "b", "y"]]),
                                      dataset(ids="text", relations=True))
    assert reference.undirected_view()["a"]["b"]["weight"] == 1
    assert reference.undirected_view(per_relation=True)["a"]["b"]["weight"] == 2


def test_self_loop_nodes_are_known(tmp_path):
    reference = oracle.Reference.read(tables(tmp_path, ["1", "2"], [["1", "1"], ["1", "2"]]), dataset())
    assert reference.self_loop_nodes() == {"1"}
