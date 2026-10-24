"""bench/corpus/tables.py: a graph/v1 document as the nodes and edges tables
warren.create_graph reads."""

import csv
import json

import pytest

from corpus import graph_v1, tables
from corpus.manifest import Dataset


def dataset(**fields):
    base = dict(name="d", tier="tiny", domain="social", ids="integer", directed=False, weighted=False,
                relations=False, nodes=3, edges=2, licence="", why="", pin=None)
    return Dataset(**{**base, **fields})


def write_graph(tmp_path, nodes, edges, **meta):
    path = tmp_path / "d.json"
    path.write_text(json.dumps({"meta": {"directed": False, "weighted": False, **meta},
                                "nodes": [{"id": n} for n in nodes], "edges": edges}))
    return path


def rows(path):
    with open(path, newline="") as f:
        return list(csv.reader(f))


@pytest.mark.parametrize("text, integer", [
    ("1", True), ("30", True), ("-4", True), ("0", True), ("9223372036854775807", True),
    ("00260881", False),  # a WordNet offset: the leading zero is part of the id
    ("-0", False), ("+1", False), ("9223372036854775808", False), ("Q42", False), ("", False),
])
def test_ids_are_integers_only_when_each_is_a_canonical_int8(text, integer):
    assert tables.is_int8(text) == integer


def test_nodes_and_edges_are_written_as_csv_with_a_load_script(tmp_path):
    path = write_graph(tmp_path, ["1", "2", "3"], [{"source": "1", "target": "2"}, {"source": "2", "target": "3"}])
    with graph_v1.open_graph(path) as graph:
        summary = tables.write(graph, dataset(), tmp_path / "out")
    assert rows(tmp_path / "out" / "nodes.csv") == [["1"], ["2"], ["3"]]
    assert rows(tmp_path / "out" / "edges.csv") == [["1", "2"], ["2", "3"]]
    load = (tmp_path / "out" / "load.sql").read_text()
    assert "CREATE TABLE nodes (id bigint PRIMARY KEY)" in load
    assert "CREATE TABLE edges (src bigint NOT NULL, dst bigint NOT NULL)" in load
    assert "\\copy edges FROM 'edges.csv'" in load
    assert summary == {"nodes": 3, "edges": 2, "ids": "integer", "relations": 0, "self_loops": 0}


def test_relations_and_weights_are_columns_of_edges(tmp_path):
    path = write_graph(tmp_path, ["a", "b"], [{"source": "a", "target": "b", "type": "knows", "weight": 2},
                                              {"source": "b", "target": "b", "type": "is", "weight": 0.5}],
                       directed=True, weighted=True)
    spec = dataset(ids="text", directed=True, weighted=True, relations=True, nodes=2)
    with graph_v1.open_graph(path) as graph:
        summary = tables.write(graph, spec, tmp_path / "out")
    assert rows(tmp_path / "out" / "edges.csv") == [["a", "b", "knows", "2"], ["b", "b", "is", "0.5"]]
    assert "CREATE TABLE edges (src text NOT NULL, dst text NOT NULL, rel text NOT NULL, weight float8 NOT NULL)" \
        in (tmp_path / "out" / "load.sql").read_text()
    assert summary["relations"] == 2 and summary["self_loops"] == 1 and summary["ids"] == "text"


def test_a_text_id_with_csv_metacharacters_survives(tmp_path):
    path = write_graph(tmp_path, ['say "hi", ok', "b"], [{"source": 'say "hi", ok', "target": "b"}])
    with graph_v1.open_graph(path) as graph:
        tables.write(graph, dataset(ids="text", nodes=2, edges=1), tmp_path / "out")
    assert rows(tmp_path / "out" / "nodes.csv")[0] == ['say "hi", ok']


def test_a_document_whose_direction_disagrees_with_the_manifest_is_refused(tmp_path):
    path = write_graph(tmp_path, ["1"], [], directed=True)
    with graph_v1.open_graph(path) as graph, pytest.raises(tables.TablesError, match="directed"):
        tables.write(graph, dataset(), tmp_path / "out")


def test_weights_are_what_the_edges_carry_not_what_the_meta_says(tmp_path):
    # london_transport's meta says weighted, but keepset's topology
    # crosswalk drops the weights: the edges decide.
    path = write_graph(tmp_path, ["1", "2"], [{"source": "1", "target": "2"}], weighted=True)
    with graph_v1.open_graph(path) as graph:
        assert tables.write(graph, dataset(nodes=2, edges=1), tmp_path / "out")["edges"] == 1
    with graph_v1.open_graph(path) as graph, pytest.raises(tables.TablesError, match="edge 0 has no weight"):
        tables.write(graph, dataset(weighted=True, nodes=2, edges=1), tmp_path / "out")


def test_ids_of_another_kind_than_the_manifest_says_are_refused(tmp_path):
    path = write_graph(tmp_path, ["01"], [])
    with graph_v1.open_graph(path) as graph, pytest.raises(tables.TablesError, match="ids are text"):
        tables.write(graph, dataset(), tmp_path / "out")


def test_relations_the_manifest_does_not_expect_are_refused(tmp_path):
    path = write_graph(tmp_path, ["1", "2"], [{"source": "1", "target": "2", "type": "r"}])
    with graph_v1.open_graph(path) as graph, pytest.raises(tables.TablesError, match="relations"):
        tables.write(graph, dataset(), tmp_path / "out")


@pytest.mark.parametrize("spec, call", [
    (dict(), "SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst FROM edges', "
              "directed := false, weighted := false, has_relations := false)"),
    (dict(directed=True, weighted=True, relations=True),
     "SELECT warren.create_graph('g', 'SELECT id FROM nodes', 'SELECT src, dst, rel, weight FROM edges', "
     "directed := true, weighted := true, has_relations := true)"),
])
def test_the_graph_is_created_as_the_manifest_describes_it(spec, call):
    assert tables.create_graph_sql(dataset(**spec), "g") == call
