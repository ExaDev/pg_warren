"""The correctness layer: pg_warren's results on a dataset against each
reference of the `oracle` port (NetworkX, igraph), as findings (dicts,
written to the results as JSON), each naming what it was checked
`against`: an oracle, or the tables themselves. pg_warren is asked once
per check, however many oracles there are. `compare` and
`compare_partitions` are exact unless given a tolerance; community
detection is held to a quality bound, not equality: pg_warren's modularity
at least the oracle's Louvain's less `slack`, each partition scored by
`warren.modularity` so both are measured alike.

`Warren` is the adapter: it reads graph `g` of a database through SQL."""

import json
import random

EXAMPLES = 5
PAGERANK_TOLERANCE = 1e-9   # max absolute difference; both converged to 1e-12
PAGERANK_STOP = 1e-12       # tol: the iteration stops when the L1 change is below N·tol
PAGERANK_DAMPING = 0.85
CLUSTERING_TOLERANCE = 1e-12
BETWEENNESS_TOLERANCE = 1e-12  # max absolute difference of normalised values
MODULARITY_TOLERANCE = 1e-9
QUALITY_SLACK = 0.02
SELF_LOOP = ("clustering counts a node with a self-loop among its own neighbours (vector_degree), NetworkX "
             "leaves the self-loop out (docs/compute.md, DESIGN.md §6.7)")
AGREEING = ("match", "documented difference")  # a finding with another status fails `check`
PATH_PAIRS = 25
PATH_SEED = 1
TABLES = "tables"  # `against` for the checks of the build against the tables themselves


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
        self.database.execute(f"SELECT warren.pagerank('g', 'pagerank', tol := {PAGERANK_STOP}, max_iter := 10000)")
        return self.vector("pagerank")

    def betweenness(self):
        """Exact (every node a source), normalised, along edges, weighted
        when the graph is: the defaults."""
        self.database.execute("SELECT warren.betweenness_centrality('g', 'betweenness')")
        return self.vector("betweenness")

    def library(self, function):
        """`triangles`, `clustering` or `core_number` into a vector of its name."""
        self.database.execute(f"SELECT warren.{function}('g', '{function}')")
        return self.vector(function)

    def community(self, function):
        """`louvain` or `leiden`: the partition and the modularity it returns."""
        q = float(self.database.query(f"SELECT warren.{function}('g', '{function}')")[0][0])
        return {node: int(value) for node, value in self.vector(function).items()}, q

    def modularity(self, labels, per_relation=False):
        """`warren.modularity` of the partition `labels` (node → integer),
        a pair in several relations counted once or `per_relation`."""
        self.database.execute(f"DROP TABLE IF EXISTS given_labels; "
                              f"CREATE TABLE given_labels (id {self.id_type}, label bigint)")
        self.database.copy_in("given_labels", sorted(labels.items()))
        self.database.execute("SELECT warren.vector_from_query('g', 'given_labels', "
                              "'SELECT id, label FROM given_labels', 'i64')")
        return float(self.database.query(
            f"SELECT warren.modularity('g', 'given_labels', per_relation := {str(per_relation).lower()})")[0][0])


def pagerank_tolerance(reference, nodes):
    """pg_warren's PageRank is NetworkX's iteration, stopped when the L1
    change is below N·tol: an iterate within N·tol·α/(1 − α) of the fixed
    point in L1. A reference that solves for the fixed point
    (`PAGERANK_SOLVED`) is allowed that much more."""
    if not reference.PAGERANK_SOLVED:
        return PAGERANK_TOLERANCE
    return PAGERANK_TOLERANCE + nodes * PAGERANK_STOP * PAGERANK_DAMPING / (1 - PAGERANK_DAMPING)


def _against(references, check, warren, answer, **options):
    """`check` of pg_warren's `warren` against `answer(reference)` of each
    reference."""
    return [{**compare(check, warren, answer(reference), **options), "against": reference.NAME}
            for reference in references]


def _path_pairs(nodes, count, seed):
    chooser = random.Random(seed)
    return [tuple(chooser.sample(nodes, 2)) for _ in range(count)]


def _path_findings(warren, references, nodes, weighted):
    pairs = {f"{source}->{target}": (source, target) for source, target in _path_pairs(nodes, PATH_PAIRS, PATH_SEED)}
    paths = {key: warren.shortest_path(*pair) for key, pair in pairs.items()}
    lengths = {key: None if path is None else len(path) - 1 for key, path in paths.items()}
    costs = {key: warren.path_cost(*pair) for key, pair in pairs.items()} if weighted else {}
    findings = []
    for reference in references:
        invalid = [key for key, path in paths.items() if path is not None and (
            (path[0], path[-1]) != pairs[key] or not all(reference.has_edge(a, b) for a, b in zip(path, path[1:])))]
        findings += [{**compare("shortest_path length", lengths,
                                {key: reference.path_length(*pair) for key, pair in pairs.items()}),
                      "against": reference.NAME},
                     {"check": "shortest_path edges", "status": "mismatch" if invalid else "match",
                      "compared": len(paths), "invalid": invalid[:EXAMPLES], "against": reference.NAME}]
        if weighted:
            findings.append({**compare("dijkstra cost", costs,
                                       {key: reference.path_cost(*pair) for key, pair in pairs.items()},
                                       tolerance=1e-9), "against": reference.NAME})
    return findings


def _modularity_finding(check, warren, oracle, note):
    return {"check": check, "status": "match" if abs(warren - oracle) <= MODULARITY_TOLERANCE else "mismatch",
            "warren": warren, "oracle": oracle, "difference": warren - oracle, "note": note}


def _community_findings(warren, references, relations):
    """Per oracle: `warren.modularity` of the oracle's Louvain partition
    against the oracle's score of it, and `louvain` and `leiden` held to
    the bound that score sets, their returned modularity rescored."""
    detected = {function: warren.community(function) for function in ("louvain", "leiden")}
    findings = []
    for reference in references:
        labels, q = reference.louvain()
        warren_on_oracle = warren.modularity(labels)
        found = [_modularity_finding("modularity definition", warren_on_oracle, q,
                                     f"warren.modularity and {reference.NAME} scoring {reference.NAME}'s Louvain "
                                     "partition")]
        if relations:
            found.append(_modularity_finding(
                "modularity per relation", warren.modularity(labels, per_relation=True),
                reference.modularity(labels, per_relation=True),
                f"per_relation := true against {reference.NAME} with A at k for an unweighted pair in k relations"))
        for function, (communities, returned) in detected.items():
            rescored = reference.modularity(communities)
            found.append(quality_bound(function, returned, warren_on_oracle))
            found.append({"check": f"{function} returned modularity",
                          "status": "match" if abs(rescored - returned) <= MODULARITY_TOLERANCE else "mismatch",
                          "warren": returned, "oracle": rescored, "difference": returned - rescored,
                          "communities": len(set(communities.values()))})
        findings += [{**f, "against": reference.NAME} for f in found]
    return findings


def run(warren, source, references, dataset, create_graph_sql):
    """Every check on `dataset`: its build against its tables (`source`),
    then each function against each reference. Returns the build's version
    row and the findings."""
    version = warren.build(create_graph_sql)
    nodes = sorted(source.nodes)
    findings = [{**compare("node_count", {"nodes": version["node_count"]}, {"nodes": len(nodes)}), "against": TABLES},
                {**compare("dropped_edges", {"edges": version["dropped_edges"]}, {"edges": 0}), "against": TABLES}]
    for direction in ("out", "in", "both") if dataset.directed else ("out",):
        findings += _against(references, f"degree {direction}", warren.degrees(direction),
                             lambda r, d=direction: r.degrees(d))
    for start in (source.hub(), random.Random(PATH_SEED).choice(nodes)):
        findings += [{**found, "source": start} for found in
                     _against(references, "bfs depths", warren.bfs_depths(start), lambda r, s=start: r.bfs_depths(s))]
    components = warren.components()
    findings += [{**compare_partitions("components", components, r.components()), "against": r.NAME}
                 for r in references]
    findings += _path_findings(warren, references, nodes, dataset.weighted)
    pagerank = warren.pagerank()
    findings += [{**compare("pagerank", pagerank, r.pagerank(), tolerance=pagerank_tolerance(r, len(nodes))),
                  "against": r.NAME} for r in references]
    findings += _against(references, "triangles", warren.library("triangles"), lambda r: r.triangles())
    findings += _against(references, "clustering", warren.library("clustering"), lambda r: r.clustering(),
                         tolerance=CLUSTERING_TOLERANCE, documented=(source.self_loop_nodes(), SELF_LOOP))
    findings += _against(references, "core_number", warren.library("core_number"), lambda r: r.core_number())
    affording = [r for r in references if len(nodes) * len(source.edges) <= r.BETWEENNESS_WORK]
    if affording:
        findings += _against(affording, "betweenness", warren.betweenness(), lambda r: r.betweenness(),
                             tolerance=BETWEENNESS_TOLERANCE)
    findings += _community_findings(warren, references, dataset.relations)
    return version, findings
