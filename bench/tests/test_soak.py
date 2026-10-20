"""bench/soak.py: the 100M-edge soak's cases, measurements and verdicts."""

import pytest

import soak

GIB = 1 << 30

STATUS = """Name:\tpostgres
VmPeak:\t 9000000 kB
VmHWM:\t 2097152 kB
VmRSS:\t 1048576 kB
RssAnon:\t  524288 kB
RssFile:\t   20000 kB
RssShmem:\t  504288 kB
"""


def test_memory_status_reads_the_peak_and_resident_sizes_in_bytes():
    assert soak.memory_status(STATUS) == {"VmHWM": 2 * GIB, "VmRSS": GIB, "RssAnon": GIB // 2,
                                          "RssFile": 20000 * 1024, "RssShmem": 504288 * 1024}


def test_cached_bytes_come_from_the_unfit_notice_of_that_version():
    notices = [
        "NOTICE:  something else",
        "psql:<stdin>:3: NOTICE:  version 7 (1234567 bytes) cannot be kept within warren.max_cache_memory "
        "(65536 bytes, 0 of them held by transient vectors), so it is reloaded on every use; raise "
        "warren.max_cache_memory",
    ]
    assert soak.cached_bytes(notices, version=7) == 1234567
    assert soak.cached_bytes(notices, version=8) is None


def test_psql_messages_are_split_by_severity():
    lines = ["psql:<stdin>:1: NOTICE:  a", "WARNING:  b", "psql:<stdin>:2: ERROR:  c", "DETAIL:  d"]
    assert soak.messages(lines) == {"notices": ["NOTICE:  a"], "warnings": ["WARNING:  b"],
                                    "errors": ["ERROR:  c", "DETAIL:  d"]}


def test_every_case_is_10m_nodes_and_100m_edges_with_its_layout():
    assert set(soak.CASES) == {"a", "b-merged", "b-unmerged"}
    for case in soak.CASES.values():
        assert (case.nodes, case.edges) == (10_000_000, 100_000_000)
    a, merged, unmerged = soak.CASES["a"], soak.CASES["b-merged"], soak.CASES["b-unmerged"]
    assert (a.weighted, a.relations, a.store_merged, a.table) == (False, 1, True, True)
    assert (merged.weighted, merged.relations, merged.store_merged, merged.table) == (True, 5, True, False)
    assert (unmerged.weighted, unmerged.relations, unmerged.store_merged, unmerged.table) == (True, 5, False, False)


def test_a_case_scales_down_for_a_quick_check():
    small = soak.CASES["b-merged"].scaled(1000)
    assert (small.nodes, small.edges, small.relations, small.store_merged) == (10_000, 100_000, 5, True)


def test_the_generator_draws_seeded_uniform_endpoints():
    sql = soak.generator_sql(soak.CASES["a"], seed=0.25)
    assert sql == ("SELECT 1 + floor(random() * 10000000)::int4 AS src, 1 + floor(random() * 10000000)::int4 AS dst "
                   "FROM (SELECT setseed(0.25) OFFSET 0) seeded CROSS JOIN generate_series(1, 100000000)")


def test_a_table_case_reads_the_edge_table():
    assert soak.edge_query(soak.CASES["a"], seed=0.25) == (
        "SELECT src::bigint, dst::bigint FROM soak_edges")


def test_a_generated_case_reads_the_generator_less_the_removed_edges_and_derives_relations_and_weights():
    case = soak.CASES["b-unmerged"]
    assert soak.edge_query(case, seed=0.25) == (
        "SELECT src::bigint, dst::bigint, 'r' || ((src # dst) % 5), (1 + (src + 3 * dst) % 9)::float8 "
        f"FROM ({soak.generator_sql(case, 0.25)}) e "
        "WHERE src % 1000 <> 7 OR NOT EXISTS (SELECT FROM soak_removed)")


def test_create_graph_passes_the_case_layout():
    sql = soak.create_graph_sql(soak.CASES["b-unmerged"], seed=0.25)
    assert "weighted := true" in sql and "has_relations := true" in sql and "store_merged := false" in sql
    assert "generate_series(1::bigint, 10000000)" in sql
    assert "has_relations := false" in soak.create_graph_sql(soak.CASES["a"], seed=0.25)


def test_segment_bytes_are_estimated_from_the_layout():
    # Per direction: offsets 8(N+1) per CSR, targets 4M and weights 8M per
    # copy of the edges; then the id map (16N) and tombstones (N/8).
    assert soak.segment_estimate(soak.CASES["a"]) == 2 * (80_000_008 + 400_000_000) + 160_000_000 + 1_250_000
    assert soak.segment_estimate(soak.CASES["b-unmerged"]) == (
        2 * (5 * 80_000_008 + 400_000_000 + 800_000_000) + 161_250_000)
    assert soak.segment_estimate(soak.CASES["b-merged"]) == (
        2 * (6 * 80_000_008 + 800_000_000 + 1_600_000_000) + 161_250_000)


def test_disk_allows_a_write_only_if_the_floor_stays_free():
    assert soak.disk_allows(free=10 * GIB, needed=5 * GIB, floor=4 * GIB)
    assert not soak.disk_allows(free=8 * GIB, needed=5 * GIB, floor=4 * GIB)


def test_pagerank_fits_the_cap_only_with_its_scratch_beside_the_graph():
    nodes = 10_000_000  # scratch ~0.88 GB
    assert soak.pagerank_fits(cached=3 * GIB, id_space=nodes, cap=4 * GIB)
    assert not soak.pagerank_fits(cached=int(3.4 * GIB), id_space=nodes, cap=4 * GIB)


MEASURED = {
    "cores": 4,
    "bfs_seconds": 0.5, "bfs_cte_seconds": 20.0,
    "reachable_edges_per_second": 80e6,
    "components_seconds": 12.0,
    "pagerank_seconds": 25.0,
    "eigen_seconds": 700.0,
    "refresh_seconds": 300.0, "refresh_peak_bytes": 3 * GIB,
    "diff_seconds": 5.0,
    "cold_load_seconds": 9.0,
    "cached_bytes": 3 * GIB, "id_space": 10_000_000, "max_cache_memory": 4 * GIB,
}


def test_verdicts_compare_each_measurement_with_its_target():
    verdicts = {v["workload"]: v for v in soak.verdicts(MEASURED)}
    assert verdicts["bfs depth 6 vs recursive CTE"]["met"] and verdicts["bfs depth 6 vs recursive CTE"][
        "measured"] == "40.0×"
    assert verdicts["reachable_set, full component"]["met"]
    assert not verdicts["components()"]["met"]
    assert verdicts["pagerank, 20 iterations"]["met"]
    assert not verdicts["eigen, k = 20, normalised Laplacian"]["met"]
    assert verdicts["refresh() incl. reverse CSR"]["met"]
    assert verdicts["diff between two versions"]["met"] and verdicts["cold load into backend"]["met"]
    assert verdicts["memory: graph + pagerank scratch within max_cache_memory"]["met"]


def test_verdicts_leave_out_what_a_case_did_not_measure():
    partial = {k: v for k, v in MEASURED.items() if k not in {"bfs_cte_seconds", "diff_seconds"}}
    names = {v["workload"] for v in soak.verdicts(partial)}
    assert "bfs depth 6 vs recursive CTE" not in names and "diff between two versions" not in names
    assert "components()" in names


def test_a_refresh_over_4gb_misses_even_when_fast():
    over = dict(MEASURED, refresh_peak_bytes=5 * GIB)
    refresh = next(v for v in soak.verdicts(over) if v["workload"] == "refresh() incl. reverse CSR")
    assert not refresh["met"]


@pytest.mark.parametrize("value, text", [(512, "512 B"), (3 * GIB, "3.22 GB"), (1_500_000, "1.50 MB")])
def test_sizes_are_written_in_decimal_units(value, text):
    assert soak.size(value) == text
