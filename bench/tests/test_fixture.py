import csv
import json

import numpy as np
import pytest

from fixtures.fixture import Fixture, Table


def edges_fixture():
    return Fixture(
        name="tiny",
        tables=(
            Table.of("nodes", id=("bigint", np.array([1, 2, 3]))),
            Table.of(
                "edges",
                src=("bigint", np.array([1, 2])),
                dst=("bigint", np.array([2, 3])),
                weight=("float8", np.array([0.5, 1.25])),
            ),
        ),
        truth={"nodes": 3, "edges": 2},
    )


def test_a_table_refuses_columns_of_different_lengths():
    with pytest.raises(ValueError, match="length"):
        Table.of("edges", src=("bigint", [1, 2]), dst=("bigint", [1]))


def test_a_fixture_finds_its_tables_by_name():
    fixture = edges_fixture()
    assert fixture.table("edges").column("dst").tolist() == [2, 3]
    with pytest.raises(KeyError):
        fixture.table("missing")


def test_write_emits_one_csv_per_table_with_a_header(tmp_path):
    edges_fixture().write(tmp_path)
    with open(tmp_path / "edges.csv", newline="") as f:
        rows = list(csv.reader(f))
    assert rows == [["src", "dst", "weight"], ["1", "2", "0.5"], ["2", "3", "1.25"]]


def test_write_emits_a_psql_load_script_for_every_table_and_the_truth(tmp_path):
    edges_fixture().write(tmp_path)
    script = (tmp_path / "load.sql").read_text()
    assert "CREATE TABLE nodes (id bigint);" in script
    assert "CREATE TABLE edges (src bigint, dst bigint, weight float8);" in script
    assert "\\copy edges FROM 'edges.csv' WITH (FORMAT csv, HEADER true)" in script
    assert "CREATE TABLE truth (key text PRIMARY KEY, value jsonb NOT NULL);" in script
    assert "\\copy truth FROM 'truth.csv' WITH (FORMAT csv, HEADER true)" in script


def test_write_emits_the_truth_as_json_and_as_key_value_rows(tmp_path):
    edges_fixture().write(tmp_path)
    assert json.loads((tmp_path / "truth.json").read_text()) == {"nodes": 3, "edges": 2}
    with open(tmp_path / "truth.csv", newline="") as f:
        rows = list(csv.reader(f))
    assert rows == [["key", "value"], ["nodes", "3"], ["edges", "2"]]


def test_cells_use_postgres_literals_for_booleans_arrays_and_nulls(tmp_path):
    Fixture(
        name="cells",
        tables=(
            Table.of(
                "t",
                flag=("boolean", np.array([True, False])),
                embedding=("float4[]", np.array([[0.5, -1.0], [2.0, 0.25]], dtype=np.float32)),
                note=("text", ["a,b", None]),
            ),
        ),
        truth={},
    ).write(tmp_path)
    with open(tmp_path / "t.csv", newline="") as f:
        rows = list(csv.reader(f))
    assert rows[1:] == [["t", "{0.5,-1}", "a,b"], ["f", "{2,0.25}", ""]]


def test_float4_cells_round_trip_exactly(tmp_path):
    values = np.array([[0.1, 1 / 3]], dtype=np.float32)
    Fixture("f", (Table.of("t", v=("float4[]", values)),), {}).write(tmp_path)
    with open(tmp_path / "t.csv", newline="") as f:
        cell = list(csv.reader(f))[1][0]
    parsed = np.array(cell.strip("{}").split(","), dtype=np.float32)
    assert np.array_equal(parsed, values[0])
