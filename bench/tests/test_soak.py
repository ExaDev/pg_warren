"""bench/soak.py: the 100M-edge soak's cases, measurements and verdicts."""

import hashlib

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
    "eigen_seconds": 700.0, "eigen_converged": True, "eigen_residual": 5e-7,
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


def test_a_capped_eigen_is_reported_per_step_not_against_the_target():
    result = {"hardware": {"cpus": 4}, "eigen_max_iter": 100,
              "stages": {"default: eigen": {"median_seconds": 50.0, "rows": ["20|0.003"]}}}
    measured = soak.summary(result)
    assert measured["eigen_seconds_per_step"] == 0.5 and "eigen_seconds" not in measured


def test_a_stopped_stage_is_a_lower_bound_and_a_miss():
    stopped = {"median_seconds": 900.0, "errors": ["ERROR:  canceling statement due to statement timeout"]}
    result = {"hardware": {"cpus": 4}, "stages": {"default: pagerank": stopped}}
    measured = soak.summary(result)
    assert measured["pagerank_seconds"] == 900.0 and measured["pagerank_seconds_stopped"]
    pagerank = next(v for v in soak.verdicts(measured) if v["workload"] == "pagerank, 20 iterations")
    assert pagerank == {"workload": "pagerank, 20 iterations", "target": "< 30 s (8 threads)",
                        "measured": "≥ 900.0 s (stopped)", "met": False}


def test_a_failed_stage_is_a_miss_carrying_its_error():
    failed = {"median_seconds": 1.0, "errors": ["ERROR:  out of memory", "DETAIL:  Failed on request of size 8"]}
    measured = soak.summary({"hardware": {"cpus": 4}, "stages": {"default: pagerank": failed}})
    assert "pagerank_seconds" not in measured
    pagerank = next(v for v in soak.verdicts(measured) if v["workload"] == "pagerank, 20 iterations")
    assert pagerank == {"workload": "pagerank, 20 iterations", "target": "< 30 s (8 threads)",
                        "measured": "failed: ERROR:  out of memory", "error": "ERROR:  out of memory",
                        "met": False}


def test_a_failed_refresh_misses_with_its_memory_error():
    failed = {"median_seconds": 99.0, "vm_hwm": 5 * GIB,
              "errors": ["ERROR:  53200: build memory limit exceeded"]}
    measured = soak.summary({"hardware": {"cpus": 4}, "stages": {"refresh": failed}})
    refresh = next(v for v in soak.verdicts(measured) if v["workload"] == "refresh() incl. reverse CSR")
    assert not refresh["met"] and refresh["error"] == "ERROR:  53200: build memory limit exceeded"


def test_planned_stages_never_reached_after_a_lost_backend_are_misses():
    result = {"hardware": {"cpus": 4}, "aborted": "psql ended during: SELECT warren.diff('soak', 1, 2)",
              "planned_stages": ["refresh", "diff", "bfs_cte"], "skipped": {"bfs_cte": "disk"},
              "stages": {"refresh": {"median_seconds": 300.0, "vm_hwm": GIB}}}
    verdicts = {v["workload"]: v for v in soak.verdicts(soak.summary(result))}
    assert verdicts["refresh() incl. reverse CSR"]["met"]
    diff = verdicts["diff between two versions"]
    assert not diff["met"]
    assert diff["error"] == "not reached: psql ended during: SELECT warren.diff('soak', 1, 2)"
    assert "bfs depth 6 vs recursive CTE" not in verdicts, "skipped for disk, not lost"
    assert "components()" not in verdicts, "never planned"


def test_a_lost_cold_load_probe_misses_the_memory_target():
    result = {"hardware": {"cpus": 4}, "aborted": "psql ended during: SELECT warren.prewarm('soak')",
              "planned_stages": ["cold_load_probe"], "skipped": {}, "stages": {}}
    verdicts = {v["workload"]: v for v in soak.verdicts(soak.summary(result))}
    assert not verdicts["memory: graph + pagerank scratch within max_cache_memory"]["met"]


class Options:
    eigen, cte, skip, cache_memory_kb = True, True, [], None


def test_the_planned_stages_follow_the_case_and_the_options():
    base = {"refresh", "cold_load_probe", "default: prewarm", "default: bfs depth 6", "default: reachable_set",
            "default: components", "default: pagerank"}
    assert soak.planned_stages(soak.CASES["a"], Options()) == sorted(base | {"default: eigen", "bfs_cte", "diff"})
    options = Options()
    options.eigen, options.skip = False, ["second-version"]
    assert soak.planned_stages(soak.CASES["b-merged"], options) == sorted(base)


def test_eigen_that_is_fast_but_not_converged_is_a_miss():
    unconverged = dict(MEASURED, eigen_seconds=60.0, eigen_converged=False, eigen_residual=1e-3)
    eigen = "eigen, k = 20, normalised Laplacian"
    missed = next(v for v in soak.verdicts(unconverged) if v["workload"] == eigen)
    assert not missed["met"] and missed["measured"] == "60.0 s, not converged, residual 1.0e-03"


def test_a_converged_eigen_is_met_though_its_explicit_residual_floors_above_tol():
    # The solver tests |β·s| ≤ tol·max|θ|; the explicit f32 residual
    # ‖Mx − θx‖ floors near 1e-7·‖M‖ (DESIGN.md §6.5), so it may exceed tol.
    converged = dict(MEASURED, eigen_seconds=16.3, eigen_converged=True, eigen_residual=1.3e-6)
    eigen = next(v for v in soak.verdicts(converged) if v["workload"].startswith("eigen"))
    assert eigen["met"] and eigen["measured"] == "16.3 s, converged, residual 1.3e-06"


def test_eigen_convergence_is_the_solvers_warning_and_the_residual_its_row():
    warning = ("WARNING:  eigen did not converge in 4100 steps; the largest residual is "
               "1.0426094828610714e-4 (raise max_iter or tol)")
    stage = {"median_seconds": 60.0, "rows": ["20|0.00010426094828610714"], "warnings": [warning]}
    measured = soak.summary({"hardware": {"cpus": 4}, "stages": {"default: eigen": stage}})
    assert measured["eigen_residual"] == pytest.approx(1.0426e-4, rel=1e-3)
    assert measured["eigen_converged"] is False
    stage = {"median_seconds": 16.3, "rows": ["20|1.3e-06"], "warnings": []}
    assert soak.summary({"hardware": {"cpus": 4}, "stages": {"default: eigen": stage}})["eigen_converged"]


def test_a_stopped_capped_eigen_is_a_lower_bound():
    stopped = {"median_seconds": 900.0, "errors": ["ERROR:  canceling statement due to statement timeout"]}
    result = {"hardware": {"cpus": 4}, "eigen_max_iter": 100, "stages": {"default: eigen": stopped}}
    eigen = next(v for v in soak.verdicts(soak.summary(result)) if v["workload"].startswith("eigen"))
    assert eigen["measured"] == "≥ 900.0 s (stopped)" and not eigen["met"]


def test_the_cte_baseline_is_timed_in_the_backend_like_bfs():
    stages = {"bfs_cte": {"median_seconds": 8.0, "server_median_seconds": 7.5},
              "default: bfs depth 6": {"median_seconds": 0.8, "server_median_seconds": 0.75}}
    measured = soak.summary({"hardware": {"cpus": 4}, "stages": stages})
    assert (measured["bfs_cte_seconds"], measured["bfs_seconds"]) == (7.5, 0.75)


def test_the_full_scale_graph_must_have_the_recorded_deduplicated_edges():
    case = soak.CASES["b-merged"]
    soak.check_edges(case, {"deduplicated_edges": 47, "edge_count": 99_999_953})
    with pytest.raises(soak.GeneratorDrift, match="48 deduplicated edges, 99999952 edges; expected 47"):
        soak.check_edges(case, {"deduplicated_edges": 48, "edge_count": 99_999_952})
    soak.check_edges(case.scaled(1000), {"deduplicated_edges": 3, "edge_count": 99_997})  # no expected value


def test_provenance_records_the_command_both_revisions_and_the_installed_library(tmp_path, monkeypatch):
    library = tmp_path / "pg_warren.so"
    library.write_bytes(b"elf")
    monkeypatch.setattr(soak, "git_revision", lambda: ("abc123", True))
    found = soak.provenance(["bench/soak.py", "a", "--no-eigen"], "495646e", "release", library)
    assert found == {
        "argv": ["bench/soak.py", "a", "--no-eigen"],
        "harness": {"revision": "abc123", "dirty": True},
        "extension": {"revision": "495646e", "profile": "release", "library": str(library),
                      "library_sha256": hashlib.sha256(b"elf").hexdigest()},
    }


class FakeSession:
    def __init__(self, log):
        self.log = log

    def __enter__(self):
        return self

    def __exit__(self, *_):
        pass

    def run(self, sql):
        self.log.append(sql)
        return {"rows": ['{"edges_traversed": 1}'] if "last_stats" in sql else ["x=1"]}

    def measure(self, sql, repeat=1):
        self.log.append(sql)
        return {"median_seconds": 1.0, "first_rows": ["1.0"] * repeat, "rows": ["1"]}


def test_the_settings_are_recorded_after_each_pass_sets_them_and_loads_the_library():
    log = []
    options = Options()
    options.threads, options.statement_timeout = 4, "15min"
    runner = soak.Soak(None, options, provenance={})
    runner.session = lambda: FakeSession(log)
    result = {"stages": {}, "cached_bytes": GIB}
    runner._queries(result, "raised", cap="8GB")
    snapshot = next(i for i, sql in enumerate(log) if "pg_settings" in sql)
    assert snapshot > max(i for i, sql in enumerate(log) if sql.startswith("SET "))
    # pg_settings lists warren.* only once a call has loaded the library.
    assert snapshot > next(i for i, sql in enumerate(log) if "warren." in sql and not sql.startswith("SET "))
    assert result["settings"] == {"raised": ["x=1"]}


def test_pagerank_counts_the_segment_rows_it_reads_in_its_own_statement():
    sql = soak.pagerank_sql("soak")
    assert ";" not in sql, "one statement: the counts are this transaction's"
    assert "warren.pagerank('soak', 'pr', tol := 0, max_iter := 20)" in sql
    # Read before, then pagerank, then read after: each step needs the one before.
    before, run, after = (sql.index(part) for part in ("AS MATERIALIZED (SELECT", "warren.pagerank", "FROM run"))
    assert before < run < after


def test_the_first_pass_runs_at_the_given_cap_and_counts_pagerank_against_it():
    log = []
    options = Options()
    options.threads, options.statement_timeout, options.cache_memory_kb = 4, None, 418945
    options.eigen = False
    runner = soak.Soak(None, options, provenance={})
    runner.session = lambda: FakeSession(log)
    runner._queries({"stages": {}, "cached_bytes": GIB}, "default", cap=runner.first_cap())
    assert "SET warren.max_cache_memory = '418945kB'" in log
    assert any("warren.pagerank" in sql and "pg_stat_get_xact" in sql for sql in log)


def test_the_first_cap_defaults_to_the_4gb_default():
    options = Options()
    options.cache_memory_kb = None
    assert soak.Soak(None, options, provenance={}).first_cap() is None
    assert soak.cap_bytes(None) == 4 * GIB
    assert soak.cap_bytes(418945) == 418945 * 1024


def test_the_summary_reports_how_many_times_pagerank_loaded_the_version():
    result = {"stages": {"default: pagerank": {"median_seconds": 3.0, "rows": ["1234|20"]}},
              "hardware": {"cpus": 4}, "version_1_segment_rows": 617, "planned_stages": [],
              "cache_memory_bytes": 418945 * 1024}
    measured = soak.summary(result)
    assert (measured["pagerank_segment_reads"], measured["pagerank_loads"]) == (1234, 2.0)
    assert measured["pagerank_seconds"] == 3.0


def test_the_cold_load_probe_lifts_the_compute_limit_so_a_version_over_both_still_loads():
    assert any("warren.max_compute_memory" in setting for setting in soak.COLD_LOAD_PROBE_SETTINGS)
