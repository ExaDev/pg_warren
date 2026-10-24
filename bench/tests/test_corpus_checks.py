"""bench/corpus/checks.py: comparing pg_warren's results with the oracle's,
and what a finding records."""

import pytest

from corpus import checks


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
