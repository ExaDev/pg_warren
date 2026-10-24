"""A graph/v1 document as the tables `warren.create_graph` reads:
`nodes (id)` and `edges (src, dst[, rel][, weight])`, written as CSV with a
psql `load.sql` (run from their directory, as a fixture's is), and the
`create_graph` call that matches the manifest's description of the graph.

Ids are `bigint` when every one is a canonical int8 (no sign on zero, no
leading zeros: `00260881` is a WordNet offset, not 260881), else `text`.
The tables hold what keepset gives, parallel edges and self-loops included;
the build deduplicates (DESIGN.md §14 item 4)."""

import csv
import re
from pathlib import Path

INTEGER = re.compile(r"-?[1-9][0-9]*|0")
INT8 = (-(1 << 63), (1 << 63) - 1)


class TablesError(ValueError):
    pass


def is_int8(text):
    """Whether id `text` is a canonical int8, so the graph's ids can be `bigint`."""
    return INTEGER.fullmatch(text) is not None and INT8[0] <= int(text) <= INT8[1]


def write(graph, dataset, directory):
    """Writes `graph` (a `graph_v1.Graph`) to `directory` as `nodes.csv`,
    `edges.csv` and `load.sql`; returns what it wrote: node and edge counts,
    the id kind, the number of relations and of self-loops. Weights are
    what the edges carry: a recipe's crosswalk may drop weights its meta
    still reports."""
    if bool(graph.meta.get("directed")) != dataset.directed:
        raise TablesError(f"{dataset.name}: keepset says directed is {graph.meta.get('directed')}, the manifest "
                          f"{dataset.directed}")
    directory = Path(directory)
    directory.mkdir(parents=True, exist_ok=True)
    nodes, integers = 0, True
    with open(directory / "nodes.csv", "w", newline="", encoding="utf-8") as f:
        out = csv.writer(f, lineterminator="\n")
        for node in graph.nodes():
            out.writerow([node["id"]])
            nodes += 1
            integers = integers and is_int8(node["id"])
    kind = "integer" if integers else "text"
    if kind != dataset.ids:
        raise TablesError(f"{dataset.name}: the ids are {kind}, the manifest says {dataset.ids}")
    edges, relations, self_loops = _write_edges(graph, dataset, directory / "edges.csv")
    if bool(relations) != dataset.relations:
        raise TablesError(f"{dataset.name}: {len(relations)} relations, the manifest says relations is "
                          f"{dataset.relations}")
    (directory / "load.sql").write_text(load_sql(dataset))
    return {"nodes": nodes, "edges": edges, "ids": kind, "relations": len(relations), "self_loops": self_loops}


def _write_edges(graph, dataset, path):
    edges, relations, self_loops = 0, set(), 0
    with open(path, "w", newline="", encoding="utf-8") as f:
        out = csv.writer(f, lineterminator="\n")
        for edge in graph.edges():
            row = [edge["source"], edge["target"]]
            if "type" in edge:
                relations.add(edge["type"])
            if dataset.relations:
                row.append(edge["type"])
            if dataset.weighted:
                if "weight" not in edge:
                    raise TablesError(f"{dataset.name}: edge {edges} has no weight")
                row.append(edge["weight"])
            out.writerow(row)
            edges += 1
            self_loops += edge["source"] == edge["target"]
    return edges, relations, self_loops


def _id_type(dataset):
    return "bigint" if dataset.ids == "integer" else "text"


def _edge_columns(dataset):
    return ["src", "dst"] + (["rel"] if dataset.relations else []) + (["weight"] if dataset.weighted else [])


def load_sql(dataset):
    id_type = _id_type(dataset)
    types = {"src": id_type, "dst": id_type, "rel": "text", "weight": "float8"}
    columns = ", ".join(f"{c} {types[c]} NOT NULL" for c in _edge_columns(dataset))
    return (f"-- {dataset.name}: keepset's graph/v1, written by bench/corpus/tables.py\n"
            f"CREATE TABLE nodes (id {id_type} PRIMARY KEY);\n"
            "\\copy nodes FROM 'nodes.csv' WITH (FORMAT csv)\n"
            f"CREATE TABLE edges ({columns});\n"
            "\\copy edges FROM 'edges.csv' WITH (FORMAT csv)\n"
            "ANALYZE nodes;\nANALYZE edges;\n")


def create_graph_sql(dataset, name):
    edges = ", ".join(_edge_columns(dataset))
    return (f"SELECT warren.create_graph('{name}', 'SELECT id FROM nodes', 'SELECT {edges} FROM edges', "
            f"directed := {_bool(dataset.directed)}, weighted := {_bool(dataset.weighted)}, "
            f"has_relations := {_bool(dataset.relations)})")


def _bool(flag):
    return "true" if flag else "false"
