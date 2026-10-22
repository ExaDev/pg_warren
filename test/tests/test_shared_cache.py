"""The shared-cache modes the SQL suite runs in (DESIGN.md §5.6): their
cluster settings."""

import pytest

from harness.shared_cache import CAP, MODES, settings


def test_off_adds_nothing():
    assert settings("off") == ()


def test_preload_loads_pg_warren_at_startup_and_sets_the_cap():
    assert settings("preload") == ("shared_preload_libraries = 'pg_warren'",
                                   f"warren.shared_cache_memory = '{CAP}'")


def test_on_sets_only_the_cap():
    assert settings("on") == (f"warren.shared_cache_memory = '{CAP}'",)


def test_every_mode_is_named():
    assert set(MODES) == {"off", "preload", "on"}
    with pytest.raises(KeyError):
        settings("shared")
