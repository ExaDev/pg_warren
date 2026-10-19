"""NetworkX reference results for oracle tests (DESIGN.md §12): export a
graph from a database, compute a reference with NetworkX, and write it back
as a table the SQL test compares against.

| computation | table | columns |
|---|---|---|
| components | oracle_components | node_id, component: the component's smallest node id (weakly connected if directed) |
| pagerank | oracle_pagerank | node_id, score (sums to 1; `--damping`, default 0.85) |
| spectrum | oracle_spectrum | i, eigenvalue: the `--k` eigenvalues of `--matrix` (adjacency, laplacian, normalized_laplacian) at the `--which` end (largest magnitude, or smallest), by a dense solver over the nodes with an edge (undirected), as `warren.eigen` defines them |
| triangles | oracle_triangles | node_id, triangles: the node's triangles in the undirected graph without self-loops |
"""

import argparse

import networkx as nx
import numpy as np

TABLES = {
    "components": ("oracle_components", "node_id bigint, component bigint"),
    "pagerank": ("oracle_pagerank", "node_id bigint, score float8"),
    "spectrum": ("oracle_spectrum", "i int, eigenvalue float8"),
    "triangles": ("oracle_triangles", "node_id bigint, triangles bigint"),
}


def export(database, nodes_sql, edges_sql, directed):
    """The graph whose nodes are `nodes_sql`'s first column and whose edges
    are `edges_sql`'s first two columns."""
    graph = nx.DiGraph() if directed else nx.Graph()
    graph.add_nodes_from(int(row[0]) for row in database.query(nodes_sql))
    graph.add_edges_from((int(row[0]), int(row[1])) for row in database.query(edges_sql))
    return graph


def compute(computation, graph, arguments):
    if computation == "components":
        parts = nx.weakly_connected_components(graph) if graph.is_directed() else nx.connected_components(graph)
        return [(node, min(part)) for part in parts for node in part]
    if computation == "pagerank":
        options = argparse.ArgumentParser(prog="pagerank")
        options.add_argument("--damping", type=float, default=0.85)
        damping = options.parse_args(arguments).damping
        return list(nx.pagerank(graph, alpha=damping, tol=1e-12, max_iter=1000).items())
    if computation == "spectrum":
        return spectrum(graph, arguments)
    if computation == "triangles":
        simple = nx.Graph(graph.to_undirected() if graph.is_directed() else graph)
        simple.remove_edges_from(list(nx.selfloop_edges(simple)))
        return list(nx.triangles(simple).items())
    raise ValueError(f"unknown oracle computation {computation!r}; expected one of {sorted(TABLES)}")


def spectrum(graph, arguments):
    options = argparse.ArgumentParser(prog="spectrum")
    options.add_argument("--matrix", default="adjacency",
                         choices=("adjacency", "laplacian", "normalized_laplacian"))
    options.add_argument("--which", default="largest", choices=("largest", "smallest"))
    options.add_argument("--k", type=int, default=1)
    args = options.parse_args(arguments)
    active = sorted(node for node in graph.nodes if graph.degree(node) > 0)
    a = nx.to_numpy_array(graph.to_undirected(), nodelist=active, weight=None)
    a[a > 0] = 1.0
    degrees = a.sum(axis=1)
    if args.matrix == "laplacian":
        a = np.diag(degrees) - a
    elif args.matrix == "normalized_laplacian":
        scale = 1 / np.sqrt(degrees)
        a = np.eye(len(active)) - scale[:, None] * a * scale[None, :]
    values = np.linalg.eigvalsh(a)
    if args.which == "largest":
        values = sorted(values, key=lambda v: (-abs(v), -v))
    return [(i + 1, float(v)) for i, v in enumerate(values[:args.k])]


def write(database, computation, rows):
    table, columns = TABLES[computation]
    database.execute(f"DROP TABLE IF EXISTS {table}; CREATE TABLE {table} ({columns})")
    database.copy_in(table, rows)


def run(database, computation, arguments):
    """Exports the graph named by `arguments`' `--nodes-sql`, `--edges-sql`
    and `--directed` (default: undirected `nodes (id)` / `edges (src, dst)`),
    computes `computation` and writes its table; the rest of `arguments`
    goes to the computation."""
    graph_options = argparse.ArgumentParser(prog=computation)
    graph_options.add_argument("--nodes-sql", default="SELECT id FROM nodes")
    graph_options.add_argument("--edges-sql", default="SELECT src, dst FROM edges")
    graph_options.add_argument("--directed", action="store_true")
    graph, rest = graph_options.parse_known_args(arguments)
    write(database, computation, compute(computation, export(database, graph.nodes_sql, graph.edges_sql,
                                                            graph.directed), rest))
