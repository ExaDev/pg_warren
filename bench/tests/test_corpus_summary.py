"""bench/corpus/summary.py: the results as Markdown tables."""

from corpus import summary


def test_correctness_is_a_row_a_dataset_with_what_did_not_match():
    results = [{"dataset": "a", "tier": "tiny", "version": {"node_count": 3, "edge_count": 4}, "seconds": 1.0,
                "findings": [{"check": "bfs depths", "status": "match"}, {"check": "pagerank", "status": "match"}]},
               {"dataset": "b", "tier": "medium", "version": {"node_count": 5, "edge_count": 6}, "seconds": 2.5,
                "findings": [{"check": "clustering", "status": "documented difference"},
                             {"check": "louvain", "status": "match", "warren": 0.4, "oracle": 0.41},
                             {"check": "leiden", "status": "match", "warren": 0.42, "oracle": 0.41}]}]
    table = summary.correctness(results).splitlines()
    assert table[0] == "| Dataset | Tier | Nodes | Stored edges | Checks matched | Not matched | Q: louvain / leiden / NetworkX Louvain | Seconds |"
    assert table[2] == "| a | tiny | 3 | 4 | 2 of 2 |  |  | 1.0 |"
    assert table[3] == "| b | medium | 5 | 6 | 2 of 3 | clustering (documented difference) | 0.400 / 0.420 / 0.410 | 2.5 |"


def test_timings_take_the_server_time_where_there_is_one():
    stages = {"refresh": {"median_seconds": 2.0, "peak_rss_anon": 3 * 2**20},
              "bfs depth 6": {"median_seconds": 0.5, "server_median_seconds": 0.25, "peak_rss_anon": 0},
              "eigen k=20": {"median_seconds": 600.0, "errors": ["ERROR:  canceling statement due to statement timeout"]}}
    assert summary.seconds(stages["refresh"]) == "2.00"
    assert summary.seconds(stages["bfs depth 6"]) == "0.250"
    assert summary.seconds(stages["eigen k=20"]) == "timeout"


def test_reads_under_ten_milliseconds_are_shown_in_milliseconds():
    assert summary.seconds({"median_seconds": 1.0, "server_median_seconds": 0.00012}) == "0.12 ms"


def test_what_did_not_match_names_the_oracle_and_each_oracles_louvain_is_shown():
    findings = [{"check": "node_count", "status": "match", "against": "tables"},
                {"check": "pagerank", "status": "match", "against": "networkx"},
                {"check": "pagerank", "status": "mismatch", "against": "igraph"},
                {"check": "louvain", "status": "match", "warren": 0.4, "oracle": 0.41, "against": "networkx"},
                {"check": "louvain", "status": "match", "warren": 0.4, "oracle": 0.43, "against": "igraph"},
                {"check": "leiden", "status": "match", "warren": 0.42, "oracle": 0.41, "against": "networkx"},
                {"check": "leiden", "status": "match", "warren": 0.42, "oracle": 0.43, "against": "igraph"}]
    row = summary.correctness([{"dataset": "a", "tier": "tiny", "version": {"node_count": 3, "edge_count": 4},
                                "seconds": 1.0, "findings": findings}]).splitlines()[2]
    assert row == "| a | tiny | 3 | 4 | 6 of 7 | pagerank against igraph (mismatch) | 0.400 / 0.420 / 0.410 (igraph 0.430) | 1.0 |"


def test_the_igraph_louvain_alone_is_shown_as_such():
    findings = [{"check": "louvain", "status": "match", "warren": 0.4, "oracle": 0.43, "against": "igraph"},
                {"check": "leiden", "status": "match", "warren": 0.42, "oracle": 0.43, "against": "igraph"}]
    row = summary.correctness([{"dataset": "a", "tier": "medium", "version": {"node_count": 3, "edge_count": 4},
                                "seconds": 1.0, "findings": findings}]).splitlines()[2]
    assert row.endswith("| 0.400 / 0.420 / (igraph 0.430) | 1.0 |")
