"""The pooler soak's growth verdict: a robust slope over the steady state."""

import random

import pytest

from harness.pooler_soak.growth import MIN_POINTS, Tolerance, judge, median_slope, theil_sen

MIB = 1 << 20


def line(slope, start=0.0, intercept=100.0, n=60, step=10.0):
    return [(start + i * step, intercept + slope * (start + i * step)) for i in range(n)]


def test_the_slope_of_a_line_is_its_slope():
    assert theil_sen(line(2.5)) == pytest.approx(2.5)
    assert theil_sen(line(0.0)) == 0


def test_the_slope_ignores_outliers_and_steps():
    points = line(1.0)
    points[10] = (points[10][0], 1e9)
    points[40] = (points[40][0], -1e9)
    assert theil_sen(points) == pytest.approx(1.0)


def test_a_long_series_is_thinned_but_keeps_its_slope():
    rng = random.Random(1)
    points = [(t, 3.0 * t + rng.uniform(-50, 50)) for t in range(5000)]
    assert theil_sen(points) == pytest.approx(3.0, rel=0.01)


def test_a_flat_series_after_warm_up_does_not_grow():
    warming = [(t, t * 1e6) for t in range(0, 60, 5)]
    flat = [(t, 60e6 + (t % 7) * 1000) for t in range(60, 600, 5)]
    found = judge(warming + flat, warmup=60, tolerance=Tolerance(2 * MIB, 0.02))
    assert not found.grew
    assert found.slope_per_hour == pytest.approx(0, abs=40_000)
    assert found.window_seconds == 535


def test_a_series_that_keeps_growing_grows_by_its_slope_over_the_window():
    found = judge(line(10_000, n=100), warmup=100, tolerance=Tolerance(2 * MIB, 0.02))
    assert found.grew
    assert found.growth == pytest.approx(10_000 * 890)
    assert found.slope_per_hour == pytest.approx(36e6)


def test_growth_within_the_tolerance_is_allowed():
    tolerance = Tolerance(absolute=1000, relative=0.1)
    level_about_5000 = [(t, 5000 + t) for t in range(0, 1000, 10)]
    found = judge(level_about_5000, warmup=0, tolerance=tolerance)
    assert found.allowed == pytest.approx(1000 + 0.1 * found.level)
    assert found.growth == pytest.approx(990)
    assert not found.grew


def test_too_few_points_in_the_steady_state_cannot_be_judged():
    assert judge(line(1.0, n=MIN_POINTS + 3), warmup=40, tolerance=Tolerance(0, 0)) is None
    assert judge(line(1.0, n=MIN_POINTS), warmup=0, tolerance=Tolerance(0, 0)) is not None


def test_backends_that_each_live_part_of_the_window_grow_by_their_median_slope():
    leaking = {pid: line(100.0, start=pid * 50, n=20) for pid in range(5)}
    assert median_slope(leaking.values()) == pytest.approx(100.0)
    assert median_slope([line(1.0, n=3)]) is None


def test_one_step_is_warm_up_not_growth():
    for at in (280, 320):
        stepped = [(t, 10 * MIB + (4 * MIB if t >= at else 0)) for t in range(0, 600, 10)]
        found = judge(stepped, warmup=0, tolerance=Tolerance(2 * MIB, 0.02))
        assert not found.grew, at
        assert found.growth > found.allowed, "the whole window alone would call it growth"


def test_growth_that_goes_on_through_both_halves_of_the_window_grows():
    stairs = [(t, 10 * MIB + (t // 100) * 2 * MIB) for t in range(0, 600, 10)]
    assert judge(stairs, warmup=0, tolerance=Tolerance(2 * MIB, 0.02)).grew
