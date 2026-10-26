"""The pooler soak's two modes: CI's short run and a long soak."""

from pooler_soak import options


def test_the_default_is_ci_s_two_minutes_sampled_every_five_seconds():
    args = options([])
    assert (args.seconds, args.sample_seconds, args.warmup_seconds, args.chaos_seconds) == (120, 5, 30, 10)
    assert args.context_every == 2
    assert args.modes == ["transaction", "session"]


def test_a_long_soak_samples_every_minute_after_half_an_hour_of_warm_up():
    args = options(["--hours", "48"])
    assert (args.seconds, args.sample_seconds, args.warmup_seconds, args.chaos_seconds) == (172800, 60, 1800, 300)
    assert args.context_every == 10, "each backend logs its memory contexts every 10 minutes"
