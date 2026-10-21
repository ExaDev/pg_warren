"""NetworkX reference results for oracle tests (DESIGN.md §12): export a
graph from a database, compute a reference with NetworkX, and write it back
as a table the SQL test compares against.

| computation | table | columns |
|---|---|---|
| components | oracle_components | node_id, component: the component's smallest node id (weakly connected if directed) |
| core_number | oracle_core_number | node_id, core: the node's core number in the undirected graph without self-loops |
| dag_longest_path | oracle_dag_longest_path | node_id, length: the longest path ending at the node (`--directed`, acyclic; weights from a third column, else 1), as `dag_longest_path_length` of the paths ending there (at least 0: NetworkX restarts after a negative prefix) |
| pagerank | oracle_pagerank | node_id, score (sums to 1; `--damping`, default 0.85) |
| spanning_forest | oracle_spanning_forest | component, weight: each component's smallest node id and the weight of its minimum spanning tree (directions ignored, the lighter of a pair's two; weights from a third column, else 1) |
| spectrum | oracle_spectrum | i, eigenvalue: the `--k` eigenvalues of `--matrix` (adjacency, laplacian, normalized_laplacian) at the `--which` end (largest magnitude, or smallest), by a dense solver over the nodes with an edge (undirected), as `warren.eigen` defines them |
| strong_components | oracle_strong_components | node_id, component: the smallest node id of its strongly connected component (`--directed`) |
| topological_order | oracle_topological_order | node_id, position: its place, from 0, in the lexicographically smallest topological order (`--directed`) |
| triangles | oracle_triangles | node_id, triangles: the node's triangles in the undirected graph without self-loops |
| eigenvector | oracle_eigenvector | node_id, score: `nx.eigenvector_centrality` (weighted when the edges have weights) |
| katz | oracle_katz | node_id, score: `nx.katz_centrality` (`--alpha`, `--beta`, `--no-normalized`; weighted when the edges have weights) |
| hits | oracle_hits | node_id, hub, authority: `nx.hits` (power iteration, `max_iter` 1000; weighted when the edges have weights) |
| label_propagation_majority | oracle_label_propagation_majority | node_id, label: `warren.label_propagation_majority`'s rule reimplemented, from each node's own id as its label (`--max-iter`, default 100) |
| clustering | oracle_clustering | node_id, coefficient: `nx.clustering` of the undirected graph without self-loops |
| louvain | oracle_louvain | node_id, community, modularity: `nx.community.louvain_communities` (`--seed`, default 1; `--resolution`, default 1; weighted when the edges have weights), each community labelled by its smallest node id, with the partition's `nx.community.modularity` on every row |
| quotient | oracle_quotient | source, target, weight: the edges of `nx.quotient_graph` by the labels of `--labels-sql` (node, label), between labels, each edge's weight the sum of the weights of the edges it joins (the count with `--count`), and a label's self-loop the sum over the edges inside it; an undirected edge counts once and is given both ways (a self-loop once) |

Edges are `--edges-sql`'s first two columns; a third column, if present, is
the edge's `weight`. A pair given more than once (the same ordered pair if
`--directed`, either order if not) is one edge whose weight is the sum, as
a build deduplicates parallel edges (DESIGN.md §14 item 4).
`spanning_forest` then joins the two directions of a directed pair keeping
the lighter, as `warren.minimum_spanning_forest` reads them.
"""

import argparse
from collections import Counter

import networkx as nx
import numpy as np

TABLES = {
    "components": ("oracle_components", "node_id bigint, component bigint"),
    "core_number": ("oracle_core_number", "node_id bigint, core bigint"),
    "dag_longest_path": ("oracle_dag_longest_path", "node_id bigint, length float8"),
    "pagerank": ("oracle_pagerank", "node_id bigint, score float8"),
    "spanning_forest": ("oracle_spanning_forest", "component bigint, weight float8"),
    "spectrum": ("oracle_spectrum", "i int, eigenvalue float8"),
    "strong_components": ("oracle_strong_components", "node_id bigint, component bigint"),
    "topological_order": ("oracle_topological_order", "node_id bigint, position bigint"),
    "triangles": ("oracle_triangles", "node_id bigint, triangles bigint"),
    "eigenvector": ("oracle_eigenvector", "node_id bigint, score float8"),
    "katz": ("oracle_katz", "node_id bigint, score float8"),
    "hits": ("oracle_hits", "node_id bigint, hub float8, authority float8"),
    "label_propagation_majority": ("oracle_label_propagation_majority", "node_id bigint, label bigint"),
    "clustering": ("oracle_clustering", "node_id bigint, coefficient float8"),
    "quotient": ("oracle_quotient", "source bigint, target bigint, weight float8"),
    "louvain": ("oracle_louvain", "node_id bigint, community bigint, modularity float8"),
}


def export(database, nodes_sql, edges_sql, directed):
    """The graph whose nodes are `nodes_sql`'s first column and whose edges
    are `edges_sql`'s first two columns, weighted by its third if it has one."""
    graph = nx.DiGraph() if directed else nx.Graph()
    graph.add_nodes_from(int(row[0]) for row in database.query(nodes_sql))
    for row in database.query(edges_sql):
        add_edge(graph, int(row[0]), int(row[1]), float(row[2]) if len(row) > 2 else None)
    return graph


def add_edge(graph, source, target, weight):
    """Adds `source → target`, summing the weights of a pair given again, as
    a build deduplicates parallel edges."""
    if weight is None:
        graph.add_edge(source, target)
    elif graph.has_edge(source, target):
        graph[source][target]["weight"] += weight
    else:
        graph.add_edge(source, target, weight=weight)


def lighter_edge(graph, source, target, weight):
    """Adds `source – target` to an undirected graph; a pair already present
    keeps the lighter weight."""
    if weight is None:
        graph.add_edge(source, target)
    elif not graph.has_edge(source, target) or weight < graph[source][target]["weight"]:
        graph.add_edge(source, target, weight=weight)


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
        hubs, authorities = nx.hits(graph, max_iter=1000)
        return [(node, hubs[node], authorities[node]) for node in graph]
    if computation == "label_propagation_majority":
        return label_propagation_majority(graph, arguments)
    if computation == "clustering":
        return list(nx.clustering(undirected_simple(graph)).items())
    if computation == "core_number":
        return list(nx.core_number(undirected_simple(graph)).items())
    if computation == "strong_components":
        return [(node, min(part)) for part in nx.strongly_connected_components(graph) for node in part]
    if computation == "topological_order":
        return [(node, i) for i, node in enumerate(nx.lexicographical_topological_sort(graph))]
    if computation == "dag_longest_path":
        return [(node, longest_path_into(graph, node)) for node in graph.nodes]
    if computation == "louvain":
        return louvain(graph, arguments)
    if computation == "spanning_forest":
        undirected = nx.Graph()
        undirected.add_nodes_from(graph.nodes)
        for source, target, data in graph.edges(data=True):
            lighter_edge(undirected, source, target, data.get("weight"))
        return [(min(part), float(nx.minimum_spanning_tree(undirected.subgraph(part)).size(weight="weight")))
                for part in nx.connected_components(undirected)]
    raise ValueError(f"unknown oracle computation {computation!r}; expected one of {sorted(TABLES)}")


def louvain(graph, arguments):
    options = argparse.ArgumentParser(prog="louvain")
    options.add_argument("--seed", type=int, default=1)
    options.add_argument("--resolution", type=float, default=1.0)
    args = options.parse_args(arguments)
    parts = nx.community.louvain_communities(graph, weight="weight", resolution=args.resolution, seed=args.seed)
    q = nx.community.modularity(graph, parts, weight="weight", resolution=args.resolution)
    return [(node, min(part), q) for part in parts for node in part]


def quotient(graph, arguments, labels=None):
    """The quotient's edges by `labels` (node → label; else read from
    `--labels-sql` by `run`), through `nx.quotient_graph`: its edge data
    sums only the edges from the first block to the second (NetworkX's
    default sums both ways in a directed graph), and a block's self-loop is
    the size of its subgraph, which `nx.quotient_graph` leaves out."""
    options = argparse.ArgumentParser(prog="quotient")
    options.add_argument("--count", action="store_true")
    options.add_argument("--labels-sql")
    weight = None if options.parse_args(arguments).count else "weight"
    blocks = {}
    for node, label in labels.items():
        blocks.setdefault(label, set()).add(node)
    label_of = {frozenset(block): label for label, block in blocks.items()}

    def between(b, c):
        return {"weight": sum(1 if weight is None else data.get(weight, 1)
                              for u, v, data in graph.edges(b, data=True) if u in b and v in c)}

    def inside(b):
        return {"inside": graph.subgraph(b).size(weight=weight)}

    q = nx.quotient_graph(graph, list(blocks.values()), edge_data=between, node_data=inside)
    rows = []
    for b, c, data in q.edges(data=True):
        rows.append((label_of[b], label_of[c], float(data["weight"])))
        if not graph.is_directed():
            rows.append((label_of[c], label_of[b], float(data["weight"])))
    rows += [(label_of[b], label_of[b], float(data["inside"])) for b, data in q.nodes(data=True)
             if graph.subgraph(b).number_of_edges() > 0]
    return rows


def longest_path_into(dag, node):
    """`nx.dag_longest_path_length` of the paths ending at `node`: its
    ancestors with one more edge out of it, heavier than any path, so the
    longest path ends with that edge, then less that edge. NetworkX restarts
    a path rather than carry a negative prefix, so this is at least 0."""
    heavy = 1 + sum(abs(data.get("weight", 1)) for _, _, data in dag.edges(data=True))
    paths = nx.DiGraph(dag.subgraph(nx.ancestors(dag, node) | {node}))
    end = max(dag.nodes) + 1
    paths.add_edge(node, end, weight=heavy)
    return float(nx.dag_longest_path_length(paths) - heavy)


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
    graph_options.add_argument("--labels-sql")
    graph, rest = graph_options.parse_known_args(arguments)
    exported = export(database, graph.nodes_sql, graph.edges_sql, graph.directed)
    if computation == "quotient":
        labels = {int(node): int(label) for node, label in database.query(graph.labels_sql)}
        rows = quotient(exported, rest, labels)
    else:
        rows = compute(computation, exported, rest)
    write(database, computation, rows)
