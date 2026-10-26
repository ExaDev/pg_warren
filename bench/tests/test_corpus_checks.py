"""bench/corpus/checks.py: comparing pg_warren's results with the oracle's,
and what a finding records."""

import networkx as nx
import pytest

from corpus import checks, oracle
from corpus.manifest import Dataset


def test_equal_values_match():
    found = checks.compare("degree", {"a": 1, "b": 2}, {"a": 1, "b": 2})
    assert found == {"check": "degree", "status": "match", "compared": 2, "mismatches": 0, "max_error": 0.0,
                     "examples": [], "only_warren": 0, "only_oracle": 0}


def test_values_within_the_tolerance_match_and_record_the_largest_error():
    found = checks.compare("pagerank", {"a": 0.5, "b": 0.5}, {"a": 0.5 + 1e-10, "b": 0.5}, tolerance=1e-9)
    assert found["status"] == "match" and found["max_error"] == pytest.approx(1e-10)


def test_a_mismatch_keeps_a_few_examples():
    warren = {str(i): i for i in range(10)}
    oracle = {str(i): i + 1 for i in range(10)}
    found = checks.compare("core_number", warren, oracle)
    assert found["status"] == "mismatch" and found["mismatches"] == 10
    assert found["examples"][0] == {"node": "0", "warren": 0, "oracle": 1}
    assert len(found["examples"]) == checks.EXAMPLES


def test_nodes_on_one_side_only_are_a_mismatch():
    found = checks.compare("bfs", {"a": 0, "b": 1}, {"a": 0, "c": 1})
    assert found["status"] == "mismatch"
    assert (found["only_warren"], found["only_oracle"]) == (1, 1)


def test_none_matches_only_none():
    assert checks.compare("path", {"x": None}, {"x": None})["status"] == "match"
    assert checks.compare("path", {"x": None}, {"x": 3})["status"] == "mismatch"


def test_partitions_match_whatever_their_labels():
    assert checks.compare_partitions("components", {"a": 7, "b": 7, "c": 9}, {"a": 0, "b": 0, "c": 1})["status"] \
        == "match"
    found = checks.compare_partitions("components", {"a": 7, "b": 7, "c": 7}, {"a": 0, "b": 0, "c": 1})
    assert found["status"] == "mismatch" and found["warren_parts"] == 1 and found["oracle_parts"] == 2


def test_a_quality_bound_holds_within_its_slack():
    found = checks.quality_bound("louvain", warren=0.41, oracle=0.42, slack=0.02)
    assert found["status"] == "match" and found["difference"] == pytest.approx(-0.01)
    assert checks.quality_bound("louvain", warren=0.30, oracle=0.42, slack=0.02)["status"] == "below bound"


def test_mismatches_all_at_documented_nodes_are_a_documented_difference():
    found = checks.compare("clustering", {"a": 0.1, "b": 0.5}, {"a": 0.2, "b": 0.5},
                           documented=({"a"}, "a self-loop counts in the degree"))
    assert found["status"] == "documented difference"
    assert found["documented"] == {"reason": "a self-loop counts in the degree", "nodes": 1}
    other = checks.compare("clustering", {"a": 0.1, "b": 0.4}, {"a": 0.2, "b": 0.5},
                           documented=({"a"}, "a self-loop counts in the degree"))
    assert other["status"] == "mismatch"


class Answering:
    """A fake pg_warren that answers as `reference` does, through the
    adapter's methods (`checks.Warren`), counting what it is asked."""

    def __init__(self, reference, source):
        self.reference, self.source, self.asked = reference, source, []

    def __getattr__(self, name):
        self.asked.append(name)
        return getattr(self, "_" + name)

    def _build(self, sql):
        return {"node_count": len(self.source.nodes), "dropped_edges": 0}

    def _degrees(self, direction):
        return self.reference.degrees(direction)

    def _bfs_depths(self, source):
        return self.reference.bfs_depths(source)

    def _components(self):
        return self.reference.components()

    def _shortest_path(self, source, target):
        import networkx as nx
        try:
            return nx.shortest_path(self.reference.graph, source, target)
        except nx.NetworkXNoPath:
            return None

    def _path_cost(self, source, target):
        return self.reference.path_cost(source, target)

    def _pagerank(self):
        return self.reference.pagerank()

    def _library(self, function):
        return getattr(self.reference, function)()

    def _betweenness(self):
        return self.reference.betweenness()

    def _community(self, function):
        return self.reference.louvain()

    def _modularity(self, labels, per_relation=False):
        return self.reference.modularity(labels, per_relation)


def lesmis_source():
    graph = nx.les_miserables_graph()
    edges = {(a, b) if a <= b else (b, a): oracle.Edge(d["weight"], d["weight"], 1)
             for a, b, d in graph.edges(data=True)}
    return oracle.Source(sorted(graph), edges, directed=False, weighted=True)


def relations_source():
    graph = nx.gnp_random_graph(60, 0.08, seed=3, directed=True)
    edges = {(str(a), str(b)): oracle.Edge(1.0 + (a + b) % 2, 1.0, 1 + (a + b) % 2) for a, b in graph.edges()}
    edges[("7", "7")] = oracle.Edge(1.0, 1.0, 1)
    return oracle.Source([str(n) for n in graph], edges, directed=True, weighted=False)


@pytest.mark.parametrize("source, relations", [(lesmis_source(), False), (relations_source(), True)])
def test_every_check_runs_against_every_oracle_and_asks_pg_warren_once(source, relations):
    references = [library(source) for library in oracle.LIBRARIES.values()]
    warren = Answering(oracle.NetworkXReference(source), source)
    dataset = Dataset(name="d", tier="tiny", domain="", ids="text", directed=source.directed,
                      weighted=source.weighted, relations=relations, nodes=0, edges=0, licence="", why="", pin=None)
    version, findings = checks.run(warren, source, references, dataset, "SELECT 1")
    assert version["node_count"] == len(source.nodes)
    assert [f for f in findings if f["status"] != "match"] == []
    by_oracle = {}
    for finding in findings:
        by_oracle.setdefault(finding["against"], []).append(finding["check"])
    assert by_oracle["tables"] == ["node_count", "dropped_edges"]
    assert by_oracle["networkx"] == by_oracle["igraph"]
    assert "betweenness" in by_oracle["igraph"]
    assert ("modularity per relation" in by_oracle["igraph"]) == relations
    asked = [name for name in warren.asked if name not in ("shortest_path", "path_cost", "modularity", "degrees")]
    assert asked == ["build", "bfs_depths", "bfs_depths", "components", "pagerank", "library", "library",
                     "library", "betweenness", "community", "community"]


def test_betweenness_is_checked_only_against_oracles_that_afford_the_graph(monkeypatch):
    source = lesmis_source()
    monkeypatch.setattr(oracle.NetworkXReference, "BETWEENNESS_WORK", len(source.nodes) * len(source.edges) - 1)
    references = [library(source) for library in oracle.LIBRARIES.values()]
    dataset = Dataset(name="d", tier="tiny", domain="", ids="text", directed=False, weighted=True, relations=False,
                      nodes=0, edges=0, licence="", why="", pin=None)
    _, findings = checks.run(Answering(references[0], source), source, references, dataset, "SELECT 1")
    assert [f["against"] for f in findings if f["check"] == "betweenness"] == ["igraph"]
    monkeypatch.setattr(oracle.IGraphReference, "BETWEENNESS_WORK", len(source.nodes) * len(source.edges) - 1)
    warren = Answering(references[0], source)
    _, findings = checks.run(warren, source, references, dataset, "SELECT 1")
    assert "betweenness" not in [f["check"] for f in findings] and "betweenness" not in warren.asked


def test_pagerank_against_a_solved_fixed_point_allows_for_where_the_iteration_stopped():
    # pg_warren and NetworkX stop when the L1 change is below N·tol, which
    # leaves them within N·tol·α/(1 − α) of the fixed point in L1; igraph
    # solves for the fixed point.
    assert checks.pagerank_tolerance(oracle.NetworkXReference, 1000) == checks.PAGERANK_TOLERANCE
    assert checks.pagerank_tolerance(oracle.IGraphReference, 1000) == \
        pytest.approx(checks.PAGERANK_TOLERANCE + 1000 * 1e-12 * 0.85 / 0.15)
