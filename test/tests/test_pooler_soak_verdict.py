"""Which sampled series the pooler soak judges for growth, and how."""

from harness.pooler_soak.verdict import BACKEND_TOLERANCES, CLUSTER_TOLERANCES, judge_rows

MIB = 1 << 20


def flat(metric, subject, level, start=0, end=600, step=10):
    return [(t, metric, subject, level + (t % 3)) for t in range(start, end, step)]


def growing(metric, subject, per_second, start=0, end=600, step=10, level=10 * MIB):
    return [(t, metric, subject, level + per_second * t) for t in range(start, end, step)]


def names(judged):
    return {(j.metric, j.subject) for j in judged if j.growth.grew}


def test_flat_series_do_not_grow():
    rows = flat("anonymous", "session/1", 20 * MIB) + flat("segments", "cluster", 40)
    judged = judge_rows(rows, warmup=60)
    assert names(judged) == set()
    assert {(j.metric, j.subject) for j in judged} >= {("anonymous", "session/1"), ("segments", "cluster"),
                                                      ("anonymous", "session (median of 1 backends)")}


def test_a_backend_that_leaks_over_the_window_grows():
    rows = growing("anonymous", "transaction/7", 50_000) + flat("anonymous", "transaction/8", 20 * MIB)
    assert ("anonymous", "transaction/7") in names(judge_rows(rows, warmup=60))


def test_a_backend_that_lives_a_short_part_of_the_window_is_judged_only_with_the_others():
    rows = growing("anonymous", "transaction/7", 50_000, start=500)
    rows += flat("anonymous", "transaction/8", 20 * MIB) + flat("anonymous", "transaction/9", 20 * MIB)
    judged = judge_rows(rows, warmup=60)
    assert ("anonymous", "transaction/7") not in {(j.metric, j.subject) for j in judged}
    assert names(judged) == set()


def test_backends_replaced_in_turn_that_all_leak_grow_by_their_median_slope():
    rows = []
    for n in range(6):
        rows += growing("warren_transient", f"session/{n}", 20_000, start=n * 100, end=n * 100 + 150, level=0)
    assert ("warren_transient", "session (median of 6 backends)") in names(judge_rows(rows, warmup=0))


def test_counts_of_leftovers_grow_by_one_per_few_samples():
    rows = [(t, "staged_versions", "cluster", t // 50) for t in range(0, 600, 10)]
    assert ("staged_versions", "cluster") in names(judge_rows(rows, warmup=60))


def test_metrics_without_a_tolerance_are_only_reported():
    rows = growing("rss", "session/1", 1_000_000) + growing("pool_total_xact_count", "session", 100)
    assert judge_rows(rows, warmup=60) == []


def test_the_tolerances_cover_the_memory_and_the_leftovers():
    assert {"anonymous", "contexts", "warren_private", "warren_transient"} <= set(BACKEND_TOLERANCES)
    assert {"shared_bytes", "dsm_segments", "activity_rows", "staged_versions", "segments",
            "segments_bytes"} <= set(CLUSTER_TOLERANCES)


def test_a_backend_born_late_has_its_own_warm_up():
    late = [(t, "anonymous", "transaction/9", 7 * MIB if t < 230 else 11 * MIB) for t in range(200, 600, 10)]
    rows = late + flat("anonymous", "transaction/8", 20 * MIB)
    judged = {(j.metric, j.subject): j.growth for j in judge_rows(rows, warmup=60)}
    assert judged[("anonymous", "transaction/9")].points == 34, "its first 60 s are its warm-up"
    assert not judged[("anonymous", "transaction/9")].grew
