"""bench/corpus/graph_v1.py: reading keepset's graph/v1 document a node and
an edge at a time."""

import json

import pytest

from corpus import graph_v1

DOCUMENT = {"meta": {"id": "g", "directed": True, "weighted": False},
            "nodes": [{"id": "a"}, {"id": "b", "type": "x"}, {"id": "c\"d"}],
            "edges": [{"source": "a", "target": "b", "type": "r"}, {"source": "b", "target": "c\"d", "weight": 2.5}]}


@pytest.mark.parametrize("indent", [None, 2])
@pytest.mark.parametrize("chunk", [1, 7, 1 << 16])
def test_a_document_reads_as_its_meta_nodes_and_edges(tmp_path, indent, chunk):
    path = tmp_path / "g.json"
    path.write_text(json.dumps(DOCUMENT, indent=indent))
    with graph_v1.open_graph(path, chunk=chunk) as graph:
        assert graph.meta == DOCUMENT["meta"]
        assert list(graph.nodes()) == DOCUMENT["nodes"]
        assert list(graph.edges()) == DOCUMENT["edges"]


def test_empty_arrays_read_as_nothing(tmp_path):
    path = tmp_path / "g.json"
    path.write_text(json.dumps({"meta": {}, "nodes": [], "edges": []}))
    with graph_v1.open_graph(path) as graph:
        assert (list(graph.nodes()), list(graph.edges())) == ([], [])


def test_the_sections_must_come_in_keepsets_order(tmp_path):
    path = tmp_path / "g.json"
    path.write_text(json.dumps({"nodes": [], "meta": {}, "edges": []}))
    with pytest.raises(graph_v1.GraphError, match='expected "meta"'):
        with graph_v1.open_graph(path):
            pass
