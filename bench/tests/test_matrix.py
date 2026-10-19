"""bench/matrix.py: which workloads run, and with what SQL."""

import json
from pathlib import Path

import matrix

WORKLOADS = [
    {"name": "refresh", "sql": "SELECT warren.refresh('g')"},
    {"name": "bfs_cte", "sql": "SELECT {source}"},
    {"name": "bfs_warren", "requires": ["warren.bfs"], "sql": "SELECT warren.bfs('g', ARRAY[{source}], 6)"},
]


def test_workloads_whose_functions_are_missing_are_skipped_with_the_reason():
    planned = matrix.plan(WORKLOADS, available={"warren.refresh"})
    assert [(w["name"], reason) for w, reason in planned] == [
        ("refresh", None), ("bfs_cte", None), ("bfs_warren", "missing warren.bfs")]


def test_workloads_run_once_their_functions_exist():
    planned = matrix.plan(WORKLOADS, available={"warren.refresh", "warren.bfs"})
    assert all(reason is None for _, reason in planned)


def test_parameters_are_substituted_by_name_leaving_other_braces_alone():
    sql = matrix.substitute("SELECT '{\"a\": 1}'::jsonb, {source}, {source}", {"source": "42"})
    assert sql == "SELECT '{\"a\": 1}'::jsonb, 42, 42"


def test_the_committed_matrix_names_a_generator_for_every_graph():
    committed = json.loads((Path(matrix.__file__).parent / "matrix.json").read_text())
    assert committed["graphs"] and committed["workloads"]
    for graph in committed["graphs"].values():
        assert graph["generate"][0] in {"random", "power-law", "grid", "sbm"}
    for workload in committed["workloads"]:
        assert set(workload) <= {"name", "sql", "requires", "repeat"}
