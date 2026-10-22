"""The shared-cache modes the SQL suite runs in (DESIGN.md §5.6): their
cluster settings."""

import pytest

from harness.shared_cache import CAP, DEFAULT_MODE, MODES, settings


def test_off_sets_the_cap_to_zero():
    assert settings("off") == ("warren.shared_cache_memory = 0",)


def test_preload_loads_pg_warren_at_startup_and_sets_the_cap():
    assert settings("preload") == ("shared_preload_libraries = 'pg_warren'",
                                   f"warren.shared_cache_memory = '{CAP}'")


def test_on_is_the_default_configuration():
    assert settings("on") == ()


def test_every_mode_is_named():
    assert set(MODES) == {"off", "preload", "on"}
    with pytest.raises(KeyError):
        settings("shared")


def test_the_suite_runs_in_the_default_configuration_unless_told_otherwise():
    assert DEFAULT_MODE == "on"
