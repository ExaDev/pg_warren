"""bench/parallel.py: the parallel measurement's statements and parsing."""

import parallel

EXPLAINED = """Finalize Aggregate (actual rows=1 loops=1)
  ->  Gather (actual rows=3 loops=1)
        Workers Planned: 2
        Workers Launched: 2
        ->  Partial Aggregate (actual rows=1 loops=3)
              ->  Parallel Seq Scan on probe (actual rows=333 loops=3)
                    Filter: warren.has_edge('g1'::text, src, dst)
Planning Time: 0.100 ms
Execution Time: 120.500 ms
Aggregate (actual rows=1 loops=1)
  ->  Seq Scan on probe (actual rows=1000 loops=1)
        Filter: warren.has_edge('g1'::text, src, dst)
Planning Time: 0.050 ms
Execution Time: 300.250 ms
"""


def test_each_run_reads_its_time_and_the_workers_it_launched():
    found = parallel.explained(EXPLAINED, 2)
    assert found["ms"] == [120.5, 300.25]
    assert found["workers_launched"] == [2, 0]
    assert found["plan"][0].startswith("Aggregate")


def test_a_summary_takes_the_median():
    found = parallel.summary({"ms": [3.0, 1.0, 2.0], "workers_launched": [2, 2, 2]})
    assert (found["median_ms"], found["min_ms"], found["max_ms"]) == (2.0, 1.0, 3.0)


def test_a_session_reads_the_graph_in_the_leader_before_it_measures():
    commands = parallel.session_commands("degree", 2, True, 3)
    assert commands[0] == "SET max_parallel_workers_per_gather = 2"
    assert "SET parallel_setup_cost = 0" in commands
    assert commands[commands.index("SELECT warren.degree('g1', 1::bigint)") + 1].startswith("EXPLAIN")
    assert sum(c.startswith("EXPLAIN") for c in commands) == 3


def test_no_workers_means_default_costs():
    assert not any("cost" in c for c in parallel.session_commands("degree", 0, True, 1))


def test_labels_name_every_flipped_function():
    assert parallel.label_sql("safe").count("PARALLEL SAFE") == len(parallel.FLIPPED)
    assert parallel.label_sql("restricted").count("PARALLEL RESTRICTED") == len(parallel.FLIPPED)
