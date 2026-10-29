"""The preload check's steps and what it allows in the server log."""

from harness.preload import STEPS, log_problems

PREFIX = "2026-10-29 10:00:00.001 UTC [7]"


def test_the_preload_is_added_and_then_removed():
    assert [name for name, _ in STEPS] == ["preloaded", "no longer preloaded"]
    added, removed = (dict(line.split(" = ") for line in settings) for _, settings in STEPS)
    assert added["shared_preload_libraries"] == "'pg_warren'"
    assert added["warren.shared_cache_memory"] != "0", "a cap, so the preloaded registry shares"
    assert removed["shared_preload_libraries"] == "''"


def test_the_strangers_refusal_and_connections_ended_by_a_restart_are_expected():
    lines = [f"{PREFIX} 42501 ERROR:  permission denied for graph social",
             f"{PREFIX} 57P01 FATAL:  terminating connection due to administrator command"]
    assert log_problems("\n".join(lines)) == []
    internal = f"{PREFIX} XX000 ERROR:  pg_warren's shared cache: broken"
    assert log_problems(internal) == [f"server log: {internal}"]
