"""A dataset's nodes and edges as pg_warren builds them, read once from its
tables for every reference: parallel edges folded into one `Edge`, an
undirected edge stored once (its ends in string order)."""

import csv
from dataclasses import dataclass


@dataclass(frozen=True)
class Edge:
    weight: float     # the relations' summed weights (1 a relation when unweighted): spmv's A
    cost: float       # the cheapest relation's weight, a path's length (§5.3)
    relations: int    # how many relations hold the pair


def _rows(path):
    with open(path, newline="", encoding="utf-8") as f:
        yield from csv.reader(f)


@dataclass(frozen=True)
class Source:
    nodes: list
    edges: dict       # (source, target) → Edge
    directed: bool
    weighted: bool

    @classmethod
    def read(cls, directory, dataset):
        nodes = [row[0] for row in _rows(directory / "nodes.csv")]
        per_pair = {}
        for row in _rows(directory / "edges.csv"):
            source, target = row[0], row[1]
            key = (source, target) if dataset.directed or source <= target else (target, source)
            weight = float(row[-1]) if dataset.weighted else 1.0
            relation = row[2] if dataset.relations else None
            # Duplicates within a relation sum (the build's deduplication).
            per_relation = per_pair.setdefault(key, {})
            per_relation[relation] = per_relation.get(relation, 0.0) + weight
        edges = {key: Edge(sum(weights.values()), min(weights.values()), len(weights))
                 for key, weights in per_pair.items()}
        return cls(nodes, edges, dataset.directed, dataset.weighted)

    def hub(self):
        """The node with the most edge ends (a self-loop's two), the larger
        id on a tie."""
        ends = dict.fromkeys(self.nodes, 0)
        for source, target in self.edges:
            ends[source] += 1
            ends[target] += 1
        return max(ends, key=lambda node: (ends[node], node))

    def self_loop_nodes(self):
        return {source for source, target in self.edges if source == target}

    def undirected_view(self, per_relation=False):
        """B's pairs, (u, v) with u ≤ v, → weight, as a library that counts
        a self-loop's weight twice in its node's degree must be given it:
        B_uu is 2·A_uu directed and A_uu undirected, so the loop gets A_uu
        directed and half of it undirected."""
        view = {}
        for (source, target), edge in self.edges.items():
            weight = edge.weight if self.weighted else float(edge.relations if per_relation else 1)
            if source == target and not self.directed:
                weight /= 2
            key = (source, target) if source <= target else (target, source)
            view[key] = view.get(key, 0.0) + weight
        return view
