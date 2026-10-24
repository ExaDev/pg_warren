"""The correctness layer: pg_warren's results on a dataset against the
NetworkX references of `oracle`, as findings (dicts, written to the
results as JSON). `compare` and `compare_partitions` are exact unless given
a tolerance; community detection is held to a quality bound, not equality:
pg_warren's modularity at least NetworkX Louvain's less `slack`, each
partition scored by `warren.modularity` so both are measured alike.

`Warren` is the adapter: it reads graph `g` of a database through SQL."""

import json
import random

EXAMPLES = 5
PAGERANK_TOLERANCE = 1e-9   # max absolute difference; both converged to 1e-12
CLUSTERING_TOLERANCE = 1e-12
MODULARITY_TOLERANCE = 1e-9
QUALITY_SLACK = 0.02
SELF_LOOP = ("clustering counts a node with a self-loop among its own neighbours (vector_degree), NetworkX "
             "leaves the self-loop out (docs/compute.md, DESIGN.md §6.7)")
PATH_PAIRS = 25
PATH_SEED = 1


def compare(check, warren, oracle, tolerance=0.0, documented=None):
    """`documented`, (nodes, reason): mismatches only at those nodes are a
    documented difference of definition, not a mismatch."""
    examples, mismatches, max_error, unexplained = [], 0, 0.0, 0
    shared = warren.keys() & oracle.keys()
    for node in sorted(shared):
        a, b = warren[node], oracle[node]
        if a is None or b is None:
            same = a is None and b is None
        else:
            error = abs(a - b)
            max_error = max(max_error, error)
            same = error <= tolerance
        if not same:
            mismatches += 1
            unexplained += documented is None or node not in documented[0]
            if len(examples) < EXAMPLES:
                examples.append({"node": node, "warren": a, "oracle": b})
    only_warren, only_oracle = len(warren.keys() - shared), len(oracle.keys() - shared)
    status = "match" if mismatches == only_warren == only_oracle == 0 else "mismatch"
    found = {"check": check, "status": status, "compared": len(shared), "mismatches": mismatches,
             "max_error": max_error, "examples": examples, "only_warren": only_warren, "only_oracle": only_oracle}
    if status == "mismatch" and unexplained == only_warren == only_oracle == 0:
        found.update(status="documented difference", documented={"reason": documented[1], "nodes": mismatches})
    return found


def _parts(labels):
    parts = {}
    for node, label in labels.items():
        parts.setdefault(label, set()).add(node)
    return {frozenset(p) for p in parts.values()}


def compare_partitions(check, warren, oracle):
    a, b = _parts(warren), _parts(oracle)
    return {"check": check, "status": "match" if a == b else "mismatch", "warren_parts": len(a),
            "oracle_parts": len(b), "differing_parts": len(a ^ b)}


def quality_bound(check, warren, oracle, slack=QUALITY_SLACK):
    return {"check": check, "status": "match" if warren >= oracle - slack else "below bound",
            "warren": warren, "oracle": oracle, "difference": warren - oracle, "slack": slack}


class Warren:
    """pg_warren's results for graph `g`, built from `nodes` and `edges`."""

    def __init__(self, database, dataset):
        self.database = database
        self.id_type = "bigint" if dataset.ids == "integer" else "text"

    def literal(self, node):
        return node + "::bigint" if self.id_type == "bigint" else "'" + node.replace("'", "''") + "'::text"

    def build(self, create_graph_sql):
        self.database.execute(create_graph_sql)
        self.database.execute("SELECT warren.refresh('g')")
        info = json.loads(self.database.query("SELECT warren.info('g')")[0][0])
        return next(v for v in info["versions"] if v["current"])

    def _pairs(self, sql):
        return {row[0]: row[1] for row in self.database.query(sql)}

    def vector(self, name):
        return {node: float(value) for node, value in
                self._pairs(f"SELECT node_id, value FROM warren.vector_values('g', '{name}', NULL::{self.id_type})").items()}

    def degrees(self, direction):
        self.database.execute(f"SELECT warren.degree_vector('g', 'degree_{direction}', '{direction}')")
        return self.vector(f"degree_{direction}")

    def bfs_depths(self, source):
        return {node: int(depth) for node, depth in self._pairs(
            f"SELECT node_id, depth FROM warren.bfs('g', ARRAY[{self.literal(source)}], NULL)").items()}

    def components(self):
        return self._pairs(f"SELECT node_id, component_id FROM warren.components('g', NULL::{self.id_type})")

    def shortest_path(self, source, target):
        # coalesce: a lone NULL is an empty line of COPY's CSV.
        found = json.loads(self.database.query(
            f"SELECT coalesce(array_to_json(warren.shortest_path('g', {self.literal(source)}, "
            f"{self.literal(target)}))::text, 'null')")[0][0])
        return None if found is None else [str(n) for n in found]

    def path_cost(self, source, target):
        rows = self.database.query(f"SELECT cost FROM warren.dijkstra('g', {self.literal(source)}, "
                                   f"{self.literal(target)})")
        return float(rows[0][0]) if rows else None

    def pagerank(self):
        self.database.execute("SELECT warren.pagerank('g', 'pagerank', tol := 1e-12, max_iter := 10000)")
        return self.vector("pagerank")

    def library(self, function):
        """`triangles`, `clustering` or `core_number` into a vector of its name."""
        self.database.execute(f"SELECT warren.{function}('g', '{function}')")
        return self.vector(function)

    def community(self, function):
        """`louvain` or `leiden`: the partition and the modularity it returns."""
        q = float(self.database.query(f"SELECT warren.{function}('g', '{function}')")[0][0])
        return {node: int(value) for node, value in self.vector(function).items()}, q

    def modularity(self, labels):
        """`warren.modularity` of the partition `labels` (node → integer)."""
        self.database.execute(f"DROP TABLE IF EXISTS given_labels; "
                              f"CREATE TABLE given_labels (id {self.id_type}, label bigint)")
        self.database.copy_in("given_labels", sorted(labels.items()))
        self.database.execute("SELECT warren.vector_from_query('g', 'given_labels', "
                              "'SELECT id, label FROM given_labels', 'i64')")
        return float(self.database.query("SELECT warren.modularity('g', 'given_labels')")[0][0])


def _path_pairs(nodes, count, seed):
    chooser = random.Random(seed)
    return [tuple(chooser.sample(nodes, 2)) for _ in range(count)]


def _path_findings(warren, reference, nodes, weighted):
    lengths_w, lengths_o, invalid, costs_w, costs_o = {}, {}, [], {}, {}
    for source, target in _path_pairs(nodes, PATH_PAIRS, PATH_SEED):
        key = f"{source}->{target}"
        path = warren.shortest_path(source, target)
        lengths_w[key] = None if path is None else len(path) - 1
        lengths_o[key] = reference.path_length(source, target)
        if path is not None and (path[0], path[-1]) != (source, target) or \
                path is not None and not all(reference.has_edge(a, b) for a, b in zip(path, path[1:])):
            invalid.append(key)
        if weighted:
            costs_w[key] = warren.path_cost(source, target)
            costs_o[key] = reference.path_cost(source, target)
    findings = [compare("shortest_path length", lengths_w, lengths_o),
                {"check": "shortest_path edges", "status": "mismatch" if invalid else "match",
                 "compared": len(lengths_w), "invalid": invalid[:EXAMPLES]}]
    if weighted:
        findings.append(compare("dijkstra cost", costs_w, costs_o, tolerance=1e-9))
    return findings


def _modularity_finding(check, warren, oracle, status_if_different, note):
    return {"check": check, "status": "match" if abs(warren - oracle) <= MODULARITY_TOLERANCE else status_if_different,
            "warren": warren, "oracle": oracle, "difference": warren - oracle, "note": note}


def _community_findings(warren, reference):
    nx_labels, nx_q = reference.louvain()
    warren_on_nx = warren.modularity(nx_labels)
    findings = [_modularity_finding("modularity definition", warren_on_nx, nx_q, "mismatch",
                                    "warren.modularity and NetworkX scoring NetworkX's Louvain partition")]
    for function in ("louvain", "leiden"):
        labels, q = warren.community(function)
        returned = reference.modularity({n: label for n, label in labels.items()})
        findings.append(quality_bound(function, q, warren_on_nx))
        findings.append({"check": f"{function} returned modularity",
                         "status": "match" if abs(returned - q) <= MODULARITY_TOLERANCE else "mismatch",
                         "warren": q, "oracle": returned, "difference": q - returned,
                         "communities": len(set(labels.values()))})
    return findings


def run(warren, reference, dataset, create_graph_sql):
    """Every check on `dataset`: its build, then each function against the
    reference. Returns the build's version row and the findings."""
    version = warren.build(create_graph_sql)
    graph = reference.graph
    nodes = sorted(graph)
    findings = [compare("node_count", {"nodes": version["node_count"]}, {"nodes": graph.number_of_nodes()}),
                compare("dropped_edges", {"edges": version["dropped_edges"]}, {"edges": 0})]
    directions = ("out", "in", "both") if dataset.directed else ("out",)
    findings += [compare(f"degree {d}", warren.degrees(d), reference.degrees(d)) for d in directions]
    hub = max(nodes, key=lambda n: (graph.degree(n), n))
    for source in (hub, random.Random(PATH_SEED).choice(nodes)):
        findings.append({**compare("bfs depths", warren.bfs_depths(source), reference.bfs_depths(source)),
                         "source": source})
    findings.append(compare_partitions("components", warren.components(), reference.components()))
    findings += _path_findings(warren, reference, nodes, dataset.weighted)
    findings.append(compare("pagerank", warren.pagerank(), reference.pagerank(), tolerance=PAGERANK_TOLERANCE))
    findings.append(compare("triangles", warren.library("triangles"), reference.triangles()))
    findings.append(compare("clustering", warren.library("clustering"), reference.clustering(),
                            tolerance=CLUSTERING_TOLERANCE, documented=(reference.self_loop_nodes(), SELF_LOOP)))
    findings.append(compare("core_number", warren.library("core_number"), reference.core_number()))
    findings += _community_findings(warren, reference)
    return version, findings
