"""Parsing a SQL test file's directives."""

import pytest

from harness.sql_test import Directives, OracleRequest


def parse(text):
    return Directives.parse(text)


def test_a_file_without_directives_needs_no_fixture_or_oracle():
    directives = parse("SELECT 1;\n")
    assert directives.fixture is None and directives.oracles == ()


def test_the_fixture_directive_holds_generate_py_arguments():
    directives = parse("-- A test.\n-- fixture: sbm --communities 4 --relations 'a,b'\nSELECT 1;\n")
    assert directives.fixture == ("sbm", "--communities", "4", "--relations", "a,b")


def test_oracle_directives_accumulate_in_order():
    directives = parse("-- oracle: components\n-- oracle: pagerank --damping 0.9 --directed\nSELECT 1;\n")
    assert directives.oracles == (
        OracleRequest("components", ()),
        OracleRequest("pagerank", ("--damping", "0.9", "--directed")),
    )


def test_directives_after_the_leading_comment_block_are_ignored():
    assert parse("SELECT 1;\n-- fixture: grid\n").fixture is None


def test_a_second_fixture_is_refused():
    with pytest.raises(ValueError, match="one fixture"):
        parse("-- fixture: grid\n-- fixture: random\n")
