"""NetworkX reference results for oracle tests (DESIGN.md §12): export a
graph from a database, compute a reference with NetworkX, and write it back
as a table the SQL test compares against.

| computation | table | columns |
|---|---|---|
| components | oracle_components | node_id, component: the component's smallest node id (weakly connected if directed) |
| pagerank | oracle_pagerank | node_id, score (sums to 1; `--damping`, default 0.85) |
| spectrum | oracle_spectrum | i, eigenvalue: the `--k` eigenvalues of `--matrix` (adjacency, laplacian, normalized_laplacian) at the `--which` end (largest magnitude, or smallest), by a dense solver over the nodes with an edge (undirected), as `warren.eigen` defines them |
| triangles | oracle_triangles | node_id, triangles: the node's triangles in the undirected graph without self-loops |
| eigenvector | oracle_eigenvector | node_id, score: `nx.eigenvector_centrality` (weighted when the edges have weights) |
| katz | oracle_katz | node_id, score: `nx.katz_centrality` (`--alpha`, `--beta`, `--no-normalized`; weighted when the edges have weights) |
| hits | oracle_hits | node_id, hub, authority: `nx.hits` (power iteration; weighted when the edges have weights) |
| label_propagation_majority | oracle_label_propagation_majority | node_id, label: `warren.label_propagation_majority`'s rule reimplemented, from each node's own id as its label (`--max-iter`, default 100) |
| clustering | oracle_clustering | node_id, coefficient: `nx.clustering` of the undirected graph without self-loops |

Edges are `--edges-sql`'s first two columns; a third column, if present, is
the edge's `weight`.
"""

import argparse
from collections import Counter

import networkx as nx
import numpy as np

TABLES = {
    "components": ("oracle_components", "node_id bigint, component bigint"),
    "pagerank": ("oracle_pagerank", "node_id bigint, score float8"),
    "spectrum": ("oracle_spectrum", "i int, eigenvalue float8"),
    "triangles": ("oracle_triangles", "node_id bigint, triangles bigint"),
    "eigenvector": ("oracle_eigenvector", "node_id bigint, score float8"),
    "katz": ("oracle_katz", "node_id bigint, score float8"),
    "hits": ("oracle_hits", "node_id bigint, hub float8, authority float8"),
    "label_propagation_majority": ("oracle_label_propagation_majority", "node_id bigint, label bigint"),
    "clustering": ("oracle_clustering", "node_id bigint, coefficient float8"),
}


def export(database, nodes_sql, edges_sql, directed):
    """The graph whose nodes are `nodes_sql`'s first column and whose edges
    are `edges_sql`'s first two columns, weighted by its third if it has one."""
    graph = nx.DiGraph() if directed else nx.Graph()
    graph.add_nodes_from(int(row[0]) for row in database.query(nodes_sql))
    for row in database.query(edges_sql):
        weight = {"weight": float(row[2])} if len(row) > 2 else {}
        graph.add_edge(int(row[0]), int(row[1]), **weight)
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
        return list(nx.triangles(undirected_simple(graph)).items())
    if computation == "eigenvector":
        return list(nx.eigenvector_centrality(graph, weight="weight").items())
    if computation == "katz":
        return katz(graph, arguments)
    if computation == "hits":
        hubs, authorities = nx.hits(graph)
        return [(node, hubs[node], authorities[node]) for node in graph]
    if computation == "label_propagation_majority":
        return label_propagation_majority(graph, arguments)
    if computation == "clustering":
        return list(nx.clustering(undirected_simple(graph)).items())
    raise ValueError(f"unknown oracle computation {computation!r}; expected one of {sorted(TABLES)}")


def undirected_simple(graph):
    simple = nx.Graph(graph.to_undirected() if graph.is_directed() else graph)
    simple.remove_edges_from(list(nx.selfloop_edges(simple)))
    return simple


def katz(graph, arguments):
    options = argparse.ArgumentParser(prog="katz")
    options.add_argument("--alpha", type=float, default=0.1)
    options.add_argument("--beta", type=float, default=1.0)
    options.add_argument("--no-normalized", dest="normalized", action="store_false")
    args = options.parse_args(arguments)
    return list(nx.katz_centrality(graph, alpha=args.alpha, beta=args.beta, normalized=args.normalized,
                                   weight="weight").items())


def label_propagation_majority(graph, arguments):
    """Synchronous majority-vote label propagation, as
    `warren.label_propagation_majority` defines it: each step every node
    takes the most frequent label among its own and its neighbours'
    (direction ignored, each neighbour once), the smallest on a tie, until
    no label changes or after `--max-iter` steps."""
    options = argparse.ArgumentParser(prog="label_propagation_majority")
    options.add_argument("--max-iter", type=int, default=100)
    max_iter = options.parse_args(arguments).max_iter
    undirected = graph.to_undirected() if graph.is_directed() else graph
    labels = {node: node for node in undirected}
    for _ in range(max_iter):
        step = {node: majority([labels[node]] + [labels[m] for m in undirected.neighbors(node)])
                for node in undirected}
        if step == labels:
            break
        labels = step
    return list(labels.items())


def majority(labels):
    counts = Counter(labels)
    most = max(counts.values())
    return min(label for label, count in counts.items() if count == most)


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
