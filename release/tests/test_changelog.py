"""Tests of CHANGELOG.md's release edits (release/changelog.py): a release
dates the section in development, the next version opens a new one, and a
release's notes are its section."""

import pytest

from changelog import ChangelogError, finalise, notes, open_next

IN_DEVELOPMENT = """# Changelog

## 0.6.0 — in development

### Added

- **`warren.set_owner`**.

## 0.5.0 — the shared cache (2026-10-23)

Milestone M9.
"""


def test_a_release_dates_the_section_in_development_and_names_it():
    released = finalise(IN_DEVELOPMENT, "0.6.0", "2026-10-24", "identity and lifecycle")
    assert "## 0.6.0 — identity and lifecycle (2026-10-24)\n\n### Added" in released
    assert "in development" not in released


def test_a_release_without_a_title_is_only_dated():
    assert "## 0.6.0 (2026-10-24)\n" in finalise(IN_DEVELOPMENT, "0.6.0", "2026-10-24")


def test_a_release_needs_its_version_in_development():
    with pytest.raises(ChangelogError, match=r"no section \"## 0\.7\.0 — in development\""):
        finalise(IN_DEVELOPMENT, "0.7.0", "2026-10-24")


def test_the_next_version_opens_above_the_release():
    opened = open_next(finalise(IN_DEVELOPMENT, "0.6.0", "2026-10-24"), "0.7.0")
    assert opened.startswith("# Changelog\n\n## 0.7.0 — in development\n\n## 0.6.0 (2026-10-24)\n")


def test_a_version_already_open_is_refused():
    with pytest.raises(ChangelogError, match=r"0\.6\.0 already has a section"):
        open_next(IN_DEVELOPMENT, "0.6.0")


def test_a_release_s_notes_are_its_section_without_its_heading():
    assert notes(IN_DEVELOPMENT, "0.6.0") == "### Added\n\n- **`warren.set_owner`**.\n"
    assert notes(IN_DEVELOPMENT, "0.5.0") == "Milestone M9.\n"


def test_notes_of_a_version_without_a_section_are_refused():
    with pytest.raises(ChangelogError, match=r"no section for 0\.4\.0"):
        notes(IN_DEVELOPMENT, "0.4.0")
