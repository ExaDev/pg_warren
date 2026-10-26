"""bench/corpus/oracle: the references pg_warren is checked against, read
from a dataset's tables under pg_warren's definitions (deduplicated edges,
neighbours counted once, the undirected view of DESIGN.md §6.7 for
modularity). Every library behind the port answers alike: each test of the
port runs against NetworkX and igraph."""

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


def source(tmp_path, nodes, edges, **fields):
    return oracle.Source.read(tables(tmp_path, nodes, edges), dataset(**fields))


@pytest.fixture(params=sorted(oracle.LIBRARIES))
def library(request):
    """Each library's reference, made from a `Source`."""
    return oracle.LIBRARIES[request.param]


# The source: what every reference is made from.

def test_parallel_edges_are_one_edge_and_their_weights_add_up(tmp_path):
    read = source(tmp_path, ["1", "2", "3"], [["1", "2", "2"], ["1", "2", "3"], ["2", "3", "1"]], weighted=True)
    assert read.edges[("1", "2")] == oracle.Edge(weight=5.0, cost=5.0, relations=1)
    assert read.nodes == ["1", "2", "3"]


def test_an_edge_in_several_relations_weighs_their_sum_and_costs_the_cheapest(tmp_path):
    read = source(tmp_path, ["a", "b"], [["a", "b", "x", "2"], ["a", "b", "y", "3"], ["a", "b", "y", "4"]],
                  ids="text", relations=True, weighted=True)
    assert read.edges == {("a", "b"): oracle.Edge(weight=9.0, cost=2.0, relations=2)}


def test_an_undirected_edge_is_stored_once_whichever_way_it_is_written(tmp_path):
    read = source(tmp_path, ["1", "2"], [["2", "1"], ["1", "2"]], directed=False)
    assert list(read.edges) == [("1", "2")]


def test_the_hub_has_the_most_edge_ends_a_self_loop_counting_twice(tmp_path):
    assert source(tmp_path, ["1", "2", "3"], [["1", "2"], ["3", "3"]]).hub() == "3"


def test_self_loop_nodes_are_known(tmp_path):
    assert source(tmp_path, ["1", "2"], [["1", "1"], ["1", "2"]]).self_loop_nodes() == {"1"}


def test_the_undirected_view_adds_an_edge_and_its_reverse(tmp_path):
    # B = A + Aᵀ: 1→2 and 2→1 make weight 2; a directed self-loop counts
    # twice in its node's degree, so a library that counts a self-loop's
    # weight twice (NetworkX, igraph) gets its weight unchanged.
    view = source(tmp_path, ["1", "2", "3"], [["1", "2"], ["2", "1"], ["2", "3"], ["3", "3"]]).undirected_view()
    assert view == {("1", "2"): 2.0, ("2", "3"): 1.0, ("3", "3"): 1.0}


def test_an_undirected_self_loop_counts_once_in_the_view(tmp_path):
    view = source(tmp_path, ["1", "2"], [["1", "2"], ["2", "2"]], directed=False).undirected_view()
    assert view[("2", "2")] == 0.5


def test_the_view_counts_a_pair_once_unless_asked_per_relation(tmp_path):
    # An unweighted pair in two relations weighs 1, as in spmv's A
    # (docs/compute.md), or 2 counted per relation.
    read = source(tmp_path, ["a", "b"], [["a", "b", "x"], ["a", "b", "y"], ["a", "b", "y"]], ids="text",
                  relations=True)
    assert read.undirected_view()[("a", "b")] == 1
    assert read.undirected_view(per_relation=True)[("a", "b")] == 2


# The port: every library answers alike.

def test_degree_counts_each_neighbour_once_a_self_loop_included(tmp_path, library):
    reference = library(source(tmp_path, ["1", "2", "3"], [["1", "2"], ["1", "1"], ["3", "1"]]))
    assert reference.degrees("out") == {"1": 2, "2": 0, "3": 1}
    assert reference.degrees("in") == {"1": 2, "2": 1, "3": 0}
    assert reference.degrees("both") == {"1": 3, "2": 1, "3": 1}


def test_undirected_degree_counts_each_neighbour_once(tmp_path, library):
    reference = library(source(tmp_path, ["1", "2", "3"], [["1", "2"], ["2", "2"], ["2", "3"]], directed=False))
    assert reference.degrees("out") == {"1": 1, "2": 3, "3": 1}


def test_bfs_depths_follow_out_edges(tmp_path, library):
    reference = library(source(tmp_path, ["1", "2", "3", "4"], [["1", "2"], ["2", "3"], ["4", "1"]]))
    assert reference.bfs_depths("1") == {"1": 0, "2": 1, "3": 2}


def test_components_are_weak(tmp_path, library):
    reference = library(source(tmp_path, ["1", "2", "3", "4", "5"], [["1", "2"], ["3", "2"], ["4", "5"]]))
    parts = {}
    for node, part in reference.components().items():
        parts.setdefault(part, set()).add(node)
    assert sorted(map(sorted, parts.values())) == [["1", "2", "3"], ["4", "5"]]


def test_paths_follow_edges_and_cost_the_cheapest_relation(tmp_path, library):
    reference = library(source(tmp_path, ["a", "b", "c"],
                               [["a", "b", "x", "5"], ["a", "b", "y", "1"], ["b", "c", "x", "2"], ["a", "c", "x", "9"]],
                               ids="text", relations=True, weighted=True))
    assert (reference.path_length("a", "c"), reference.path_cost("a", "c")) == (1, 3.0)
    assert (reference.path_length("c", "a"), reference.path_cost("c", "a")) == (None, None)
    assert reference.has_edge("a", "b") and not reference.has_edge("b", "a")


def test_pagerank_of_a_directed_graph_with_a_dangling_node_and_a_self_loop(tmp_path, library):
    edges = [["1", "2"], ["2", "3"], ["3", "1"], ["3", "4"], ["1", "1"]]
    reference = library(source(tmp_path, ["1", "2", "3", "4"], edges))
    expected = nx.pagerank(nx.DiGraph([tuple(e) for e in edges]), tol=1e-14, max_iter=10_000)
    assert reference.pagerank() == pytest.approx(expected, abs=1e-10)


def test_undirected_pagerank_follows_each_edge_both_ways_and_a_self_loop_once(tmp_path, library):
    edges = [["1", "2", "1"], ["2", "3", "2"], ["3", "3", "4"]]
    reference = library(source(tmp_path, ["1", "2", "3"], edges, directed=False, weighted=True))
    graph = nx.Graph()
    graph.add_weighted_edges_from((a, b, float(w)) for a, b, w in edges)
    expected = nx.pagerank(graph, weight="weight", tol=1e-14, max_iter=10_000)
    assert reference.pagerank() == pytest.approx(expected, abs=1e-10)


def test_triangles_clustering_and_cores_are_of_the_undirected_simple_graph(tmp_path, library):
    reference = library(source(tmp_path, ["1", "2", "3", "4"],
                               [["1", "2"], ["2", "3"], ["3", "1"], ["1", "3"], ["3", "3"], ["3", "4"]]))
    assert reference.triangles() == {"1": 1, "2": 1, "3": 1, "4": 0}
    assert reference.core_number() == {"1": 2, "2": 2, "3": 2, "4": 1}
    assert reference.clustering() == pytest.approx({"1": 1.0, "2": 1.0, "3": 1 / 3, "4": 0.0})


def test_betweenness_is_normalised_along_edges(tmp_path, library):
    # A directed path 1→2→3→4: 2 lies on (1,3) and (1,4), 3 on (1,4) and
    # (2,4); normalised by (n − 1)(n − 2) = 6. A self-loop is on no path.
    reference = library(source(tmp_path, ["1", "2", "3", "4"], [["1", "2"], ["2", "3"], ["3", "4"], ["4", "4"]]))
    assert reference.betweenness() == pytest.approx({"1": 0.0, "2": 2 / 6, "3": 2 / 6, "4": 0.0})


def test_undirected_betweenness_is_normalised_as_networkx(tmp_path, library):
    graph = nx.barbell_graph(4, 1)
    edges = [[str(a), str(b)] for a, b in graph.edges()]
    reference = library(source(tmp_path, [str(n) for n in graph], edges, directed=False))
    expected = nx.betweenness_centrality(graph, normalized=True)
    assert reference.betweenness() == pytest.approx({str(n): v for n, v in expected.items()}, abs=1e-12)


def test_weighted_betweenness_takes_the_cheapest_paths(tmp_path, library):
    # 1→3 directly costs 5, through 2 costs 2: 2 is on that path.
    reference = library(source(tmp_path, ["1", "2", "3"], [["1", "3", "5"], ["1", "2", "1"], ["2", "3", "1"]],
                               weighted=True))
    assert reference.betweenness() == pytest.approx({"1": 0.0, "2": 0.5, "3": 0.0})


def test_modularity_of_a_partition_is_of_the_undirected_view(tmp_path, library):
    reference = library(source(tmp_path, ["1", "2", "3", "4"], [["1", "2"], ["3", "4"]], directed=False))
    assert reference.modularity({"1": 0, "2": 0, "3": 1, "4": 1}) == pytest.approx(0.5)


def test_modularity_counts_an_undirected_self_loop_once_in_its_nodes_degree(tmp_path, library):
    edges = [["1", "2"], ["2", "3"], ["3", "1"], ["3", "4"], ["4", "5"], ["5", "6"], ["6", "4"], ["6", "6"]]
    reference = library(source(tmp_path, [str(n) for n in range(1, 7)], edges, directed=False))
    # m = 7.5 (the self-loop counts once in 2m = 15); degrees 2, 2, 3 | 3, 2, 3.
    expected = (3 + 3.5) / 7.5 - (7 / 15) ** 2 - (8 / 15) ** 2
    assert reference.modularity({"1": 0, "2": 0, "3": 0, "4": 1, "5": 1, "6": 1}) == pytest.approx(expected)


def test_modularity_per_relation_weighs_a_pair_by_its_relations(tmp_path, library):
    edges = [["a", "b", "x"], ["a", "b", "y"], ["c", "d", "x"], ["b", "c", "x"]]
    reference = library(source(tmp_path, ["a", "b", "c", "d"], edges, ids="text", relations=True))
    labels = {"a": 0, "b": 0, "c": 1, "d": 1}
    # Per relation a–b weighs 2: m = 4, degrees 2 + 3 | 2 + 1.
    assert reference.modularity(labels, per_relation=True) == pytest.approx(3 / 4 - (5 / 8) ** 2 - (3 / 8) ** 2)
    assert reference.modularity(labels) == pytest.approx(2 / 3 - (3 / 6) ** 2 - (3 / 6) ** 2)


def test_louvain_returns_a_partition_and_its_modularity(tmp_path, library):
    edges = [[str(a), str(b)] for a, b in nx.barbell_graph(5, 0).edges()]
    reference = library(source(tmp_path, [str(n) for n in range(10)], edges, directed=False))
    labels, q = reference.louvain()
    assert len(set(labels.values())) == 2
    assert q == pytest.approx(reference.modularity(labels))


def test_every_library_is_named(library):
    assert oracle.LIBRARIES[library.NAME] is library
