"""Whether a sampled series grows in the steady state (DESIGN.md §13 M11,
"watching for memory growth").

The steady state is every sample from the end of the warm-up on. A
series' growth is its Theil–Sen slope (the median of the slopes between
every pair of samples, so a few outliers or a step do not move it) times
the steady window's length: what it gained, robustly, while the workload
was the same. It *grew* when that is more than its `Tolerance` (an
absolute amount plus a fraction of its median level in the window), and
so is the growth at each half's own slope: growth that goes on, not one
step (a backend that first builds a version, or first runs a kernel on
its threads, keeps the allocator's arenas it then took). A longer run is
therefore stricter: a leak has longer to add up, and the tolerance does
not scale with time. Backends that each live only part of
the window (a pooler replaces a server connection a client broke) are
judged together by the median of their slopes (`median_slope`)."""

import statistics
from dataclasses import dataclass

MIN_POINTS = 6
HALF_POINTS = 3
MAX_POINTS = 300  # longer series are thinned evenly: the slope is quadratic in the samples
SECONDS_PER_HOUR = 3600


@dataclass(frozen=True)
class Tolerance:
    absolute: float
    relative: float

    def allowed(self, level):
        return self.absolute + self.relative * abs(level)


@dataclass(frozen=True)
class Growth:
    points: int
    window_seconds: float
    level: float
    slope_per_hour: float
    growth: float
    allowed: float
    halves: tuple = ()  # the growth over the window at each half's slope; empty if a half has too few samples

    @property
    def grew(self):
        return self.growth > self.allowed and all(half > self.allowed for half in self.halves)


def thinned(points):
    if len(points) <= MAX_POINTS:
        return list(points)
    step = (len(points) - 1) / (MAX_POINTS - 1)
    return [points[round(i * step)] for i in range(MAX_POINTS)]


def theil_sen(points):
    """The median of the slopes between every pair of `points` ((t, value),
    by t) at different times; 0 if there is no such pair."""
    points = thinned(sorted(points))
    slopes = [(v2 - v1) / (t2 - t1) for i, (t1, v1) in enumerate(points) for t2, v2 in points[i + 1:] if t2 != t1]
    return statistics.median(slopes) if slopes else 0


def judge(points, warmup, tolerance):
    """The growth of series `points` ((elapsed seconds, value)) after
    `warmup` seconds, or None when the steady state has fewer than
    `MIN_POINTS` samples."""
    steady = sorted(p for p in points if p[0] >= warmup)
    if len(steady) < MIN_POINTS:
        return None
    window = steady[-1][0] - steady[0][0]
    slope = theil_sen(steady)
    level = statistics.median(v for _, v in steady)
    middle = steady[0][0] + window / 2
    halves = ([p for p in steady if p[0] <= middle], [p for p in steady if p[0] >= middle])
    growth = tuple(theil_sen(half) * window for half in halves) if min(map(len, halves)) >= HALF_POINTS else ()
    return Growth(len(steady), window, level, slope * SECONDS_PER_HOUR, slope * window, tolerance.allowed(level),
                  growth)


def median_slope(series):
    """The median of the Theil–Sen slopes of the `series` that have at
    least `MIN_POINTS` samples, or None if none has."""
    slopes = [theil_sen(points) for points in series if len(points) >= MIN_POINTS]
    return statistics.median(slopes) if slopes else None
