"""The source of the concurrent refresh check (DESIGN.md §5.4, §7) and its
oracle. The graph's source tables `public.nodes (id)` and `public.edges
(src, dst, w)` go through numbered *generations*: generation 0 is
`first`, and generation k is generation k − 1 changed by `edit(…, k, …)`,
a deterministic function of both, so any session can work out any
generation's tables. An editor moves the tables on one generation in one
transaction, which also sets `public.state.gen`, so every snapshot sees
the tables of exactly the generation it reads there.

The node and edge queries add a marker: a hub node 0 with one edge, to
node `MARK + gen`. So every version names the generation it was built
from (`warren.neighbours(graph, 0)`), and a read can be checked against
`oracle(source, gen)`, the graph a refresh of that generation builds:
nodes 1..n/2 are never removed, the rest come and go (tombstones, and
edges dropped for their missing endpoints)."""

import heapq
import random
import threading
from dataclasses import dataclass, field

MARK = 1_000_000_000
NODE_QUERY = f"SELECT id FROM public.nodes UNION ALL SELECT 0 UNION ALL SELECT {MARK} + gen FROM public.state"
EDGE_QUERY = (f"SELECT src, dst, w FROM public.edges UNION ALL SELECT 0, {MARK} + gen, 1 FROM public.state")
CHANGED_EDGES = 6


@dataclass(frozen=True)
class Source:
    """The source tables at one generation: node ids, and each edge's weight."""
    nodes: frozenset
    edges: dict = field(hash=False)


@dataclass(frozen=True)
class Edit:
    removed_edges: tuple
    added_edges: dict = field(hash=False)
    removed_nodes: frozenset = frozenset()
    added_nodes: frozenset = frozenset()

    def apply(self, source):
        edges = {e: w for e, w in source.edges.items() if e not in set(self.removed_edges)}
        edges.update(self.added_edges)
        return Source((source.nodes - self.removed_nodes) | self.added_nodes, edges)


def first(nodes):
    """Generation 0: a ring 1 → 2 → … → n → 1 with a chord from each node."""
    edges = {}
    for node in range(1, nodes + 1):
        for target in (node % nodes + 1, (node * 7919) % nodes + 1):
            if target != node:
                edges[(node, target)] = float(1 + (node * 31 + target) % 10)
    return Source(frozenset(range(1, nodes + 1)), edges)


def edit(source, gen, nodes):
    """The edit that makes generation `gen` from `source`, generation
    `gen − 1`: some edges removed, some added, and one removable node
    (above n/2) removed or one brought back."""
    rng = random.Random(gen)
    removed = tuple(rng.sample(sorted(source.edges), min(CHANGED_EDGES, len(source.edges))))
    added = {}
    while len(added) < CHANGED_EDGES:
        edge = (rng.randint(1, nodes), rng.randint(1, nodes))
        if edge[0] != edge[1] and (edge not in source.edges or edge in removed):
            added[edge] = float(rng.randint(1, 10))
    added = {e: w for e, w in added.items() if e not in removed}
    node = rng.randint(nodes // 2 + 1, nodes)
    if node in source.nodes:
        return Edit(removed, added, removed_nodes=frozenset({node}))
    return Edit(removed, added, added_nodes=frozenset({node}))


class Generations:
    """Every generation's source, worked out in order and remembered; safe
    to share between threads."""

    def __init__(self, nodes):
        self.nodes = nodes
        self._sources = [first(nodes)]
        self._lock = threading.Lock()

    def source(self, gen):
        with self._lock:
            while len(self._sources) <= gen:
                before = self._sources[-1]
                self._sources.append(edit(before, len(self._sources), self.nodes).apply(before))
            return self._sources[gen]

    def edit(self, gen):
        """The edit that makes generation `gen`."""
        return edit(self.source(gen - 1), gen, self.nodes)


@dataclass
class Graph:
    """What a version built from one generation holds (`oracle`)."""
    adjacency: dict
    dropped_edges: int

    @property
    def node_count(self):
        return len(self.adjacency)

    @property
    def edge_count(self):
        return sum(len(targets) for targets in self.adjacency.values())

    @property
    def weight_sum(self):
        return sum(sum(targets.values()) for targets in self.adjacency.values())

    def out(self, node):
        return sorted(self.adjacency.get(node, {}))

    def depths(self, start, depth):
        """Each node within `depth` hops of `start` (any distance if None), it
        included, with its distance in hops."""
        seen, frontier, hops = {start: 0}, [start], 0
        while frontier and (depth is None or hops < depth):
            hops += 1
            frontier = list(dict.fromkeys(t for n in frontier for t in self.adjacency[n] if t not in seen))
            seen.update((node, hops) for node in frontier)
        return seen

    def reachable(self, start, depth):
        """The nodes within `depth` hops of `start` (any distance if None), it included."""
        return set(self.depths(start, depth))

    def ends(self, start, hops):
        """The nodes some walk of exactly `hops` hops from `start` ends on."""
        ends = {start}
        for _ in range(hops):
            ends = {t for n in ends for t in self.adjacency[n]}
        return ends

    def cost(self, start, target):
        """The least summed weight of a path from `start` to `target`, or None."""
        best, queue = {start: 0.0}, [(0.0, start)]
        while queue:
            cost, node = heapq.heappop(queue)
            if node == target:
                return cost
            if cost > best[node]:
                continue
            for neighbour, weight in self.adjacency[node].items():
                if cost + weight < best.get(neighbour, float("inf")):
                    best[neighbour] = cost + weight
                    heapq.heappush(queue, (cost + weight, neighbour))
        return None


def oracle(source, gen):
    """The graph a refresh of generation `gen`, whose tables are `source`, builds."""
    live = source.nodes | {0, MARK + gen}
    adjacency = {node: {} for node in live}
    adjacency[0][MARK + gen] = 1.0
    dropped = 0
    for (src, dst), weight in source.edges.items():
        if src in live and dst in live:
            adjacency[src][dst] = weight
        else:
            dropped += 1
    return Graph(adjacency, dropped)


def setup_sql(source):
    """Creates and fills the source tables at generation 0, `source`."""
    nodes = ", ".join(f"({n})" for n in sorted(source.nodes))
    edges = ", ".join(f"({s}, {d}, {w})" for (s, d), w in sorted(source.edges.items()))
    return (f"CREATE TABLE public.nodes (id bigint PRIMARY KEY);\n"
            f"CREATE TABLE public.edges (src bigint, dst bigint, w float8, PRIMARY KEY (src, dst));\n"
            f"CREATE TABLE public.state (gen int NOT NULL);\n"
            f"INSERT INTO public.nodes VALUES {nodes};\n"
            f"INSERT INTO public.edges VALUES {edges};\n"
            f"INSERT INTO public.state VALUES (0);\n")


def edit_sql(change, gen):
    """The statements that apply `change` and record generation `gen`."""
    statements = []
    if change.removed_edges:
        pairs = ", ".join(f"({s}, {d})" for s, d in change.removed_edges)
        statements.append(f"DELETE FROM public.edges WHERE (src, dst) IN ({pairs})")
    if change.added_edges:
        rows = ", ".join(f"({s}, {d}, {w})" for (s, d), w in sorted(change.added_edges.items()))
        statements.append(f"INSERT INTO public.edges VALUES {rows}")
    if change.removed_nodes:
        statements.append(f"DELETE FROM public.nodes WHERE id IN ({', '.join(map(str, sorted(change.removed_nodes)))})")
    if change.added_nodes:
        rows = ", ".join(f"({n})" for n in sorted(change.added_nodes))
        statements.append(f"INSERT INTO public.nodes VALUES {rows}")
    statements.append(f"UPDATE public.state SET gen = {gen}")
    return ";\n".join(statements) + "\n"
