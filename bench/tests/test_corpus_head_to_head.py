"""bench/corpus/head_to_head: pg_warren timed against igraph and Neo4j
(Cypher, and the Graph Data Science library) on the suite's datasets.
The runner's logic (cold and warm runs, budgets, timeouts, answers checked
before a time counts) is tested against a fake engine; the answers against
small graphs whose results are known."""

import csv
import hashlib

import networkx as nx
import pytest

from corpus import oracle
from corpus.head_to_head import answers, pagerank, report, timing, workloads
from corpus.head_to_head.engine import EngineError, Timeout
from corpus.head_to_head.workloads import Workload
from corpus.manifest import Dataset


def dataset(**fields):
    base = dict(name="d", tier="tiny", domain="social", ids="integer", directed=True, weighted=False,
                relations=False, nodes=0, edges=0, licence="", why="", pin=None)
    return Dataset(**{**base, **fields})


def source(tmp_path, nodes, edges, **fields):
    for name, rows in (("nodes", [[n] for n in nodes]), ("edges", edges)):
        with open(tmp_path / f"{name}.csv", "w", newline="") as f:
            csv.writer(f).writerows(rows)
    return oracle.Source.read(tmp_path, dataset(**fields))


class Clock:
    """A clock each engine call advances by the seconds it is given."""

    def __init__(self):
        self.now = 0.0

    def __call__(self):
        return self.now


class FakeEngine:
    name = "fake"

    def __init__(self, clock, seconds=1.0, answer=7, unsupported=None, fail=None):
        self.clock, self.seconds, self._answer = clock, seconds, answer
        self.unsupported_reason, self.fail = unsupported, fail
        self.calls = []

    def describe(self):
        return {"version": "1", "threads": 1}

    def load(self):
        self.calls.append("load")
        return {"ingest": [2.0], "build": [3.0, 1.0, 2.0]}

    def unsupported(self, workload):
        return self.unsupported_reason

    def make_cold(self, workload):
        self.calls.append("cold")
        return {"restart": 4.0}

    def reset(self, workload):
        self.calls.append("reset")

    def run(self, workload):
        self.calls.append("run")
        if self.fail:
            raise self.fail
        seconds = self.seconds.pop(0) if isinstance(self.seconds, list) else self.seconds
        self.clock.now += seconds

    def answer(self, workload):
        self.calls.append("answer")
        return self._answer


def agree(workload, answer):
    return {"status": "match" if answer == 7 else "mismatch"}


REACH = Workload("bfs depth 6", "reach", source="1", depth=6)


# The timing: cold, then warm, the answer checked before a time counts.

def test_a_workload_runs_cold_once_then_warm_and_reports_the_warm_median():
    clock = Clock()
    engine = FakeEngine(clock, seconds=[5.0, 1.0, 3.0, 2.0])
    measured = timing.measure(engine, REACH, agree, repeat=3, budget=100, clock=clock)
    assert measured["cold_seconds"] == 5.0
    assert measured["warm_seconds"] == [1.0, 3.0, 2.0]
    assert measured["warm_median_seconds"] == 2.0
    assert measured["cold_phases"] == {"restart": 4.0}
    assert measured["check"] == {"status": "match"} and measured["counted"]


def test_the_engine_is_made_cold_once_and_reset_before_every_run():
    clock = Clock()
    engine = FakeEngine(clock)
    timing.measure(engine, REACH, agree, repeat=2, budget=100, clock=clock)
    assert engine.calls == ["cold", "reset", "run", "reset", "run", "reset", "run", "answer"]


def test_warm_runs_stop_once_the_budget_is_spent_but_one_always_runs():
    clock = Clock()
    measured = timing.measure(FakeEngine(clock, seconds=60.0), REACH, agree, repeat=5, budget=100, clock=clock)
    assert measured["warm_seconds"] == [60.0]


def test_an_unsupported_workload_is_recorded_with_its_reason_and_not_run():
    clock = Clock()
    engine = FakeEngine(clock, unsupported="not expressible in Cypher")
    measured = timing.measure(engine, REACH, agree, repeat=3, budget=100, clock=clock)
    assert measured == {"status": "unsupported", "reason": "not expressible in Cypher", "counted": False}
    assert engine.calls == []


def test_a_timeout_is_recorded_and_nothing_more_runs():
    clock = Clock()
    engine = FakeEngine(clock, fail=Timeout("600 s"))
    measured = timing.measure(engine, REACH, agree, repeat=3, budget=100, clock=clock)
    assert measured == {"status": "timeout", "reason": "600 s", "counted": False, "cold_phases": {"restart": 4.0}}
    assert engine.calls == ["cold", "reset", "run"]


def test_an_engine_error_is_recorded_as_an_error():
    clock = Clock()
    measured = timing.measure(FakeEngine(clock, fail=EngineError("boom")), REACH, agree, repeat=3, budget=100,
                              clock=clock)
    assert (measured["status"], measured["reason"], measured["counted"]) == ("error", "boom", False)


def test_a_time_whose_answer_does_not_match_does_not_count():
    clock = Clock()
    measured = timing.measure(FakeEngine(clock, answer=8), REACH, agree, repeat=1, budget=100, clock=clock)
    assert measured["check"]["status"] == "mismatch" and not measured["counted"]


def test_a_documented_difference_still_counts():
    clock = Clock()
    measured = timing.measure(FakeEngine(clock), REACH, lambda w, a: {"status": "documented difference"},
                              repeat=1, budget=100, clock=clock)
    assert measured["counted"]


def test_an_engine_run_loads_then_measures_every_workload():
    clock = Clock()
    engine = FakeEngine(clock)
    result = timing.run(engine, [REACH, Workload("components", "components")], agree, repeat=1, budget=100,
                        clock=clock)
    assert result["describe"] == {"version": "1", "threads": 1}
    assert result["load"] == {"ingest": {"seconds": [2.0], "median_seconds": 2.0},
                              "build": {"seconds": [3.0, 1.0, 2.0], "median_seconds": 2.0}}
    assert list(result["workloads"]) == ["bfs depth 6", "components"]


# The workloads: what each dataset runs.

def test_the_start_of_a_directed_graph_has_the_most_out_edges(tmp_path):
    # Node 3 has the most edge ends, but only receives.
    read = source(tmp_path, ["1", "2", "3"], [["1", "3"], ["2", "3"], ["1", "2"]])
    assert workloads.start(read) == "1"


def test_the_start_of_an_undirected_graph_is_its_hub(tmp_path):
    read = source(tmp_path, ["1", "2", "3"], [["1", "3"], ["2", "3"]], directed=False)
    assert workloads.start(read) == "3"


def test_every_dataset_runs_the_core_set_and_betweenness_only_where_affordable(tmp_path):
    read = source(tmp_path, ["1", "2", "3"], [["1", "2"], ["2", "3"]])
    names = [w.name for w in workloads.plan(read, betweenness_work=100)]
    assert names == ["degree", "bfs depth 6", "bfs depths", "shortest paths", "pagerank 20 iterations",
                     "components", "triangles", "louvain", "leiden", "betweenness"]
    assert "betweenness" not in [w.name for w in workloads.plan(read, betweenness_work=5)]


def test_shortest_paths_are_seeded_pairs_and_weighted_when_the_graph_is(tmp_path):
    read = source(tmp_path, ["1", "2", "3", "4"], [["1", "2", "2"], ["2", "3", "1"]], weighted=True)
    paths = next(w for w in workloads.plan(read) if w.kind == "paths")
    assert paths.weighted and len(paths.pairs) == workloads.PATH_PAIRS
    assert all(a != b for a, b in paths.pairs)
    assert paths == next(w for w in workloads.plan(read) if w.kind == "paths")


# The answers: each engine's against the reference.

@pytest.fixture
def chain(tmp_path):
    """1 → 2 → 3 → 1, 3 → 4; 5 alone."""
    read = source(tmp_path, ["1", "2", "3", "4", "5"], [["1", "2"], ["2", "3"], ["3", "1"], ["3", "4"]])
    return answers.Reference(read, oracle.IGraphReference(read))


def test_reach_counts_the_nodes_within_the_depth(chain):
    assert chain.check(Workload("r", "reach", source="1", depth=1), 2)["status"] == "match"
    assert chain.check(Workload("r", "reach", source="1", depth=1), 3)["status"] == "mismatch"


def test_depths_and_paths_are_compared_exactly(chain):
    assert chain.check(Workload("d", "depths", source="1"), {"1": 0, "2": 1, "3": 2, "4": 3})["status"] == "match"
    pairs = (("1", "4"), ("4", "1"))
    assert chain.check(Workload("p", "paths", pairs=pairs), {"1->4": 3, "4->1": None})["status"] == "match"
    assert chain.check(Workload("p", "paths", pairs=pairs), {"1->4": 2, "4->1": None})["status"] == "mismatch"


def test_components_are_compared_as_partitions(chain):
    assert chain.check(Workload("c", "components"), {"1": 9, "2": 9, "3": 9, "4": 9, "5": 0})["status"] == "match"
    assert chain.check(Workload("c", "components"), {"1": 9, "2": 9, "3": 9, "4": 1, "5": 0})["status"] == "mismatch"


def test_triangles_are_per_node_or_a_total(chain):
    per_node = {"1": 1, "2": 1, "3": 1, "4": 0, "5": 0}
    assert chain.check(Workload("t", "triangles"), per_node)["status"] == "match"
    assert chain.check(Workload("t", "triangles"), {"total": 1})["status"] == "match"
    assert chain.check(Workload("t", "triangles"), {"total": 2})["status"] == "mismatch"


def test_degree_differs_as_documented_only_at_self_loops(tmp_path):
    read = source(tmp_path, ["1", "2"], [["1", "2"], ["2", "2"]], directed=False)
    reference = answers.Reference(read, oracle.IGraphReference(read))
    found = reference.check(Workload("g", "degree"), {"1": 1, "2": 3})
    assert found["status"] == "documented difference"


def test_communities_are_held_to_the_quality_bound_and_scored_alike(tmp_path):
    two_triangles = [["1", "2"], ["2", "3"], ["3", "1"], ["4", "5"], ["5", "6"], ["6", "4"], ["3", "4"]]
    read = source(tmp_path, [str(n) for n in range(1, 7)], two_triangles, directed=False)
    reference = answers.Reference(read, oracle.IGraphReference(read))
    good = reference.check(Workload("l", "louvain"), {"1": 0, "2": 0, "3": 0, "4": 1, "5": 1, "6": 1})
    bad = reference.check(Workload("l", "louvain"), {str(n): 0 for n in range(1, 7)})
    assert good["status"] == "match" and good["communities"] == 2
    assert good["modularity"] == pytest.approx(5 / 14)
    assert bad["status"] == "below bound"


def test_betweenness_is_compared_normalised(chain):
    graph = nx.DiGraph([("1", "2"), ("2", "3"), ("3", "1"), ("3", "4")])
    graph.add_node("5")
    expected = nx.betweenness_centrality(graph)
    assert chain.check(Workload("b", "betweenness"), expected)["status"] == "match"


def test_pagerank_is_checked_under_the_convention_the_engine_computes(chain):
    read = chain.source
    for convention in pagerank.CONVENTIONS:
        scores = pagerank.CONVENTIONS[convention](read, iterations=20)
        found = chain.check(Workload("p", "pagerank", iterations=20), (convention, scores))
        assert found["status"] == "match", convention
    off = dict(pagerank.normalised(read, iterations=20), **{"1": 0.5})
    assert chain.check(Workload("p", "pagerank", iterations=20), ("normalised", off))["status"] == "mismatch"


# PageRank's conventions.

def test_the_normalised_iteration_converges_to_networkx(tmp_path):
    edges = [["1", "2"], ["2", "3"], ["3", "1"], ["3", "4"], ["1", "1"]]
    read = source(tmp_path, ["1", "2", "3", "4"], edges)
    expected = nx.pagerank(nx.DiGraph([tuple(e) for e in edges]), tol=1e-15, max_iter=10_000)
    assert pagerank.normalised(read, iterations=500) == pytest.approx(expected, abs=1e-12)
    assert pagerank.fixed_point(read) == pytest.approx(expected, abs=1e-12)


def test_twenty_normalised_iterations_are_networkx_stopped_at_twenty(tmp_path):
    # NetworkX starts at 1/N and spreads a dangling node's rank evenly, as
    # pg_warren's library does (docs/compute.md).
    edges = [["1", "2"], ["2", "3"], ["3", "1"], ["3", "4"]]
    read = source(tmp_path, ["1", "2", "3", "4"], edges)
    graph = nx.DiGraph([tuple(e) for e in edges])
    with pytest.raises(nx.PowerIterationFailedConvergence):
        nx.pagerank(graph, tol=0, max_iter=20)
    x = {n: 0.25 for n in graph}
    for _ in range(20):
        dangling = 0.85 * x["4"] / 4
        x = {n: 0.15 / 4 + dangling + 0.85 * sum(x[u] / graph.out_degree(u) for u in graph.predecessors(n))
             for n in graph}
    assert pagerank.normalised(read, iterations=20) == pytest.approx(x, abs=1e-15)


def test_the_unnormalised_iteration_starts_at_one_less_damping_and_drops_dangling_rank(tmp_path):
    # GDS: x₀ = 1 − d, x = (1 − d) + d·Aᵀ(x / out); a dangling node's rank is lost.
    read = source(tmp_path, ["1", "2"], [["1", "2"]])
    assert pagerank.unnormalised(read, iterations=1) == pytest.approx({"1": 0.15, "2": 0.15 + 0.85 * 0.15})
    assert pagerank.unnormalised(read, iterations=0) == pytest.approx({"1": 0.15, "2": 0.15})


def test_undirected_pagerank_follows_each_edge_both_ways(tmp_path):
    read = source(tmp_path, ["1", "2", "3"], [["1", "2", "1"], ["2", "3", "3"]], directed=False, weighted=True)
    graph = nx.Graph()
    graph.add_weighted_edges_from([("1", "2", 1.0), ("2", "3", 3.0)])
    expected = nx.pagerank(graph, weight="weight", tol=1e-15, max_iter=10_000)
    assert pagerank.normalised(read, iterations=500) == pytest.approx(expected, abs=1e-12)


# The report.

def test_the_report_shows_a_time_only_when_its_answer_agreed():
    result = {"dataset": "d", "engines": {
        "pg_warren": {"workloads": {"bfs depth 6": {"cold_seconds": 0.5, "warm_median_seconds": 0.0012,
                                                    "counted": True, "check": {"status": "match"}}}},
        "neo4j": {"workloads": {"bfs depth 6": {"cold_seconds": 2.0, "warm_median_seconds": 1.5,
                                                "counted": False, "check": {"status": "mismatch"}}}},
        "igraph": {"workloads": {"bfs depth 6": {"status": "unsupported", "reason": "no", "counted": False}}}}}
    table = report.workloads([result], ["pg_warren", "neo4j", "igraph"])
    row = next(line for line in table.splitlines() if line.startswith("| d | bfs depth 6"))
    assert row == "| d | bfs depth 6 | 0.500 / 1.20 ms | mismatch | — |"


def test_seconds_read_at_a_glance():
    assert [report.seconds(s) for s in (0.00042, 0.25, 3.14159, 125.0)] == ["0.42 ms", "0.250", "3.14", "125"]


# The engines' downloads are pinned.

def test_a_download_that_does_not_match_its_pin_is_refused(tmp_path):
    from corpus.head_to_head import neo4j_server
    path = tmp_path / "x.jar"
    path.write_bytes(b"bytes")
    pin = {"sha256": hashlib.sha256(b"bytes").hexdigest(), "bytes": 5}
    neo4j_server.verify(path, pin)
    with pytest.raises(neo4j_server.PinMismatch):
        neo4j_server.verify(path, {**pin, "sha256": "0" * 64})


def test_the_neo4j_configuration_replaces_the_shipped_settings_it_sets():
    from corpus.head_to_head import neo4j_server
    shipped = "# comment\nserver.http.enabled=true\nserver.jvm.additional=-XX:+UseG1GC\n"
    written = neo4j_server.configuration(shipped, {"server.http.enabled": "false", "dbms.security.auth_enabled": "false"})
    assert written.splitlines() == ["# comment", "server.jvm.additional=-XX:+UseG1GC", "server.http.enabled=false",
                                    "dbms.security.auth_enabled=false"]


def test_the_import_reads_the_suites_tables_with_headers_of_their_shape():
    from corpus.head_to_head import neo4j_server
    assert neo4j_server.headers(dataset()) == ("id:ID", ":START_ID,:END_ID")
    assert neo4j_server.headers(dataset(relations=True, weighted=True)) == ("id:ID", ":START_ID,:END_ID,:TYPE,weight:double")


def test_the_import_names_the_id_type_and_a_relationship_type_only_when_the_tables_have_none(tmp_path):
    from corpus.head_to_head import neo4j_server
    plain = neo4j_server.import_arguments(dataset(), tmp_path / "d", tmp_path / "h")
    typed = neo4j_server.import_arguments(dataset(ids="text", relations=True), tmp_path / "d", tmp_path / "h")
    assert "--id-type=integer" in plain and "--id-type=string" in typed
    assert f"--relationships=LINK={tmp_path}/h/edges.header,{tmp_path}/d/edges.csv" in plain
    assert f"--relationships={tmp_path}/h/edges.header,{tmp_path}/d/edges.csv" in typed
    assert f"--nodes=Node={tmp_path}/h/nodes.header,{tmp_path}/d/nodes.csv" in plain


# The engines' queries.

def test_cypher_follows_out_edges_of_a_directed_graph_and_both_ways_otherwise():
    from corpus.head_to_head import cypher_engine
    reach = Workload("bfs depth 6", "reach", source="1", depth=6)
    assert "-[*0..6]->(n)" in cypher_engine.queries(reach, dataset()).timed
    assert "-[*0..6]-(n)" in cypher_engine.queries(reach, dataset(directed=False)).timed


def test_cypher_cannot_run_the_iterative_workloads_or_a_weighted_path():
    from corpus.head_to_head import cypher_engine
    for kind in ("pagerank", "components", "louvain", "leiden", "betweenness"):
        assert cypher_engine.queries(Workload(kind, kind), dataset()) is None
        assert cypher_engine.UNSUPPORTED[kind]
    assert cypher_engine.queries(Workload("p", "paths", pairs=(("1", "2"),), weighted=True),
                                 dataset(weighted=True)) is None


def test_gds_reads_the_undirected_projection_for_triangles_and_communities_of_a_directed_graph():
    from corpus.head_to_head import gds_engine
    assert gds_engine.graph_for(Workload("t", "triangles"), dataset()) == "u"
    assert gds_engine.graph_for(Workload("l", "leiden"), dataset()) == "u"
    assert gds_engine.graph_for(Workload("p", "pagerank"), dataset()) == "g"
    assert gds_engine.graph_for(Workload("t", "triangles"), dataset(directed=False)) == "g"


def test_gds_pagerank_runs_the_same_propagation_steps_and_reads_weights_when_there_are_some():
    from corpus.head_to_head import gds_engine
    call = gds_engine.mutate_call(Workload("p", "pagerank", iterations=20), dataset(weighted=True), 4)
    assert "maxIterations: 21" in call and "tolerance: 0.0" in call
    assert "relationshipWeightProperty: 'weight'" in call and "concurrency: 4" in call
    assert "relationshipWeightProperty" not in gds_engine.mutate_call(
        Workload("p", "pagerank", iterations=20), dataset(), 4)


def test_gds_betweenness_is_rescaled_to_networkx_normalisation():
    from corpus.head_to_head import gds_engine
    assert gds_engine.betweenness_scale(5, directed=True) == 1 / 12
    assert gds_engine.betweenness_scale(5, directed=False) == 2 / 12


def test_pg_warren_reads_text_ids_as_quoted_literals_and_weighted_paths_through_dijkstra():
    from corpus.head_to_head import warren_engine
    assert warren_engine.literal("it's", "text") == "'it''s'::text"
    assert warren_engine.literal("42", "bigint") == "42::bigint"
    weighted = warren_engine.paths_sql(Workload("p", "paths", pairs=(("1", "2"),), weighted=True), "bigint")
    assert "warren.dijkstra('g', p.s, p.t)" in weighted
    assert "warren.shortest_path" in warren_engine.paths_sql(Workload("p", "paths", pairs=(("1", "2"),)), "bigint")


@pytest.mark.parametrize("directed", [True, False])
def test_the_igraph_engine_answers_every_workload_as_the_reference_does(tmp_path, directed):
    from corpus.head_to_head import igraph_engine
    edges = [["1", "2", "2"], ["2", "3", "1"], ["3", "1", "1"], ["3", "4", "5"], ["4", "5", "1"], ["5", "6", "1"],
             ["6", "4", "2"], ["1", "2", "1"], ["7", "8", "1"]]
    nodes = [str(n) for n in range(1, 10)]
    read = source(tmp_path, nodes, edges, directed=directed, weighted=True)
    described = dataset(directed=directed, weighted=True)
    reference = answers.Reference(read, oracle.IGraphReference(read))
    engine = igraph_engine.IGraphEngine(described, tmp_path)
    result = timing.run(engine, workloads.plan(read), reference.check, repeat=2, budget=10)
    statuses = {name: m.get("check", m)["status"] for name, m in result["workloads"].items()}
    assert set(statuses.values()) == {"match"}, statuses
    assert set(result["load"]) == {"ingest", "build"}
