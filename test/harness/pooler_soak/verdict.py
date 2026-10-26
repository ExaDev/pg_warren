"""Which sampled series the pooler soak judges for growth
(harness.pooler_soak.growth), with what tolerance, and how backends that
live only part of the steady window are judged.

Per server backend (`BACKEND_TOLERANCES`): its private anonymous memory
(smaps_rollup's `Anonymous`), its memory contexts' total, and
pg_warren's private and transient bytes. A backend that lives at least
half the steady window is judged alone; every backend with enough
samples is also judged with the others of its pool, by the median of
their slopes over the whole window, so a leak shows even when the pool
replaces its backends. A backend that a pool started later has a
warm-up of its own, as long as the run's from its first sample. Per
cluster (`CLUSTER_TOLERANCES`): the shared
cache's bytes and entries, the DSM segments mapped, the activity and
progress rows, the versions (promoted and staged), segments, vectors
and chunks, and the catalog tables' sizes. The tolerances leave room for
what varies without leaking: version sizes as the source changes,
staged versions in flight, the dead tuples autovacuum has not yet
reclaimed. Other series (RSS, PSS, the pools' counters) are reported."""

import statistics
from collections import defaultdict
from dataclasses import dataclass

from .growth import MIN_POINTS, SECONDS_PER_HOUR, Growth, Tolerance, judge, median_slope

MIB = 1 << 20
BACKEND_TOLERANCES = {
    "anonymous": Tolerance(4 * MIB, 0.05),
    "contexts": Tolerance(2 * MIB, 0.05),
    "warren_private": Tolerance(1 * MIB, 0.10),
    "warren_transient": Tolerance(1 * MIB, 0.10),
}
CLUSTER_TOLERANCES = {
    "shared_bytes": Tolerance(16 * MIB, 0.25),
    "shared_entries": Tolerance(4, 0.25),
    "dsm_segments": Tolerance(4, 0.25),
    "dsm_bytes": Tolerance(16 * MIB, 0.25),
    "activity_rows": Tolerance(4, 0),
    "progress_rows": Tolerance(2, 0),
    "promoted_versions": Tolerance(2, 0),
    "staged_versions": Tolerance(2, 0),
    "segments": Tolerance(8, 0.25),
    "vectors": Tolerance(16, 0.25),
    "vector_chunks": Tolerance(32, 0.25),
    "graphs_bytes": Tolerance(4 * MIB, 0.5),
    "versions_bytes": Tolerance(4 * MIB, 0.5),
    "segments_bytes": Tolerance(32 * MIB, 0.5),
    "vectors_bytes": Tolerance(4 * MIB, 0.5),
    "vector_chunks_bytes": Tolerance(8 * MIB, 0.5),
}


@dataclass(frozen=True)
class Judged:
    metric: str
    subject: str
    growth: Growth


def series(rows):
    """Rows `(elapsed, metric, subject, value)` as points by (metric, subject)."""
    found = defaultdict(list)
    for elapsed, metric, subject, value in rows:
        found[(metric, subject)].append((float(elapsed), float(value)))
    return found


def judge_rows(rows, warmup):
    """Every judged series' growth after `warmup` seconds."""
    by_series = series(rows)
    if not by_series:
        return []
    end = max(t for points in by_series.values() for t, _ in points)
    window = end - warmup
    judged = []
    pools = defaultdict(list)
    for (metric, subject), points in sorted(by_series.items()):
        if subject == "cluster" and metric in CLUSTER_TOLERANCES:
            found = judge(points, warmup, CLUSTER_TOLERANCES[metric])
            if found:
                judged.append(Judged(metric, subject, found))
        elif metric in BACKEND_TOLERANCES and "/" in subject:
            own_warmup = max(warmup, min(t for t, _ in points) + warmup)
            steady = sorted(p for p in points if p[0] >= own_warmup)
            pools[(metric, subject.split("/")[0])].append(steady)
            if len(steady) >= MIN_POINTS and steady[-1][0] - steady[0][0] >= window / 2:
                judged.append(Judged(metric, subject, judge(steady, own_warmup, BACKEND_TOLERANCES[metric])))
    for (metric, mode), backends in sorted(pools.items()):
        found = _median_of_backends(backends, window, BACKEND_TOLERANCES[metric])
        if found:
            judged.append(Judged(metric, f"{mode} (median of {found.points} backends)", found))
    return judged


def _median_of_backends(backends, window, tolerance):
    """The pool's growth: the median of its backends' slopes over the
    whole window, at the median of their levels; `points` counts the
    backends."""
    sampled = [points for points in backends if len(points) >= MIN_POINTS]
    slope = median_slope(sampled)
    if slope is None:
        return None
    level = statistics.median(v for points in sampled for _, v in points)
    return Growth(len(sampled), window, level, slope * SECONDS_PER_HOUR, slope * window, tolerance.allowed(level))


def failures(judged):
    return [f"{j.metric} of {j.subject} grew by {j.growth.growth:,.0f} in the steady state "
            f"({j.growth.slope_per_hour:,.0f}/h at a level of {j.growth.level:,.0f}; "
            f"{j.growth.allowed:,.0f} allowed)" for j in judged if j.growth.grew]
