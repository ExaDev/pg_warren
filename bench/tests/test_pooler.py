"""bench/pooler.py: the pooler benchmark's scripts, configuration, parsing and budget."""

import pooler

GIB = 1 << 30
MIB = 1 << 20

PGBENCH = """pgbench (16.15 (Ubuntu 16.15-0ubuntu0.24.04.1))
transaction type: /tmp/neighbours-one-graph.sql
number of clients: 10
number of transactions actually processed: 2812345
number of failed transactions: 0 (0.000%)
latency average = 0.071 ms
initial connection time = 12.345 ms
tps = 140617.250000 (without initial connection time)
"""

STATUS = "Name:\tpostgres\nVmHWM:\t  204800 kB\nRssAnon:\t  102400 kB\nRssFile:\t 1000 kB\nRssShmem:\t 51200 kB\n"
SMAPS = "Rss:  160000 kB\nPss:  120000 kB\nPss_Anon:  100000 kB\nPss_File:  1000 kB\nPss_Shmem:  19000 kB\n"


def test_one_graph_reads_one_graph_and_own_graph_one_per_connection():
    assert pooler.graph_names("one-graph", 50) == ["g1"]
    assert pooler.graph_names("own-graph", 3) == ["own1", "own2", "own3"]


def test_scripts_draw_a_random_node_and_name_the_graph_each_client_reads():
    neighbours = pooler.script("neighbours", 1000, "one-graph")
    assert neighbours.startswith("\\set n random(1, 1000)\n")
    assert "warren.neighbours('g1', :n::bigint)" in neighbours
    bfs = pooler.script("bfs", 1000, "own-graph")
    assert "warren.bfs('own' || (:client_id + 1), ARRAY[:n::bigint], 3)" in bfs


def test_pgbouncer_pools_transactions_over_the_clusters_socket():
    config = pooler.pgbouncer_config("/c/pgbouncer", "/c", "5432", 10, "postgres")
    assert "pooler = host=/c port=5432 dbname=pooler" in config
    assert "pool_mode = transaction" in config
    assert "default_pool_size = 10" in config
    assert "unix_socket_dir = /c/pgbouncer" in config
    assert "listen_addr =\n" in config


def test_pgbench_reports_throughput_latency_and_counts():
    assert pooler.pgbench_summary(PGBENCH) == {"tps": 140617.25, "latency_ms": 0.071,
                                               "transactions": 2812345, "failed": 0}
    assert pooler.pgbench_summary("pgbench: error: connection refused") == {}


def test_a_backends_memory_comes_from_status_and_smaps_in_bytes():
    assert pooler.memory_fields(STATUS, SMAPS) == {
        "VmHWM": 200 * MIB, "RssAnon": 100 * MIB, "RssShmem": 50 * MIB,
        "Pss": 120000 * 1024, "Pss_Anon": 100000 * 1024, "Pss_Shmem": 19000 * 1024}


def test_totals_count_the_shared_versions_once():
    backends = [{"RssAnon": 100, "Pss": 150}, {"RssAnon": 300, "Pss": 350}]
    assert pooler.totals(backends, 1000) == {"backends": 2, "rss_anon": 400, "pss": 500, "shared_versions": 1000,
                                             "private_and_shared": 1400, "max_rss_anon": 300}
    assert pooler.totals([], 0)["max_rss_anon"] == 0


def test_private_copies_are_estimated_per_backend_and_graph_read():
    assert pooler.private_estimate("one-graph", 50, 1, GIB) == 50 * GIB
    assert pooler.private_estimate("own-graph", 50, 50, 12 * MIB) == 50 * 12 * MIB


def test_only_private_runs_past_the_budget_are_skipped():
    assert pooler.skip_reason("shared", "one-graph", 50, 1, GIB, 8 * GIB) is None
    assert pooler.skip_reason("off", "one-graph", 4, 1, GIB, 8 * GIB) is None
    reason = pooler.skip_reason("off", "one-graph", 50, 1, GIB, 8 * GIB)
    assert reason.startswith("private copies need about 53.69 GB")


def test_latencies_summarise_the_sessions():
    assert pooler.latencies([0.5, 0.1, 0.2]) == {"seconds": [0.5, 0.1, 0.2], "median": 0.2, "max": 0.5, "min": 0.1}
    assert pooler.latencies([]) == {}


def test_a_mode_sets_the_cap():
    assert pooler.shared_cache_sql("0") == "ALTER SYSTEM SET warren.shared_cache_memory = '0'"
