"""Generated calls: each argument of a function is filled from what its
name and type say it is (a graph, a vector, a version, a node of the
graph's id type, a source query, a direction...), drawn from the world's
graphs, versions and vectors, or, with probability `hostility`, from the
hostile values of its type (`values`). The first graph an argument names
is the context of the arguments after it, so a node has that graph's id
type and a version is one of its versions, unless hostility says
otherwise."""

from .fixture import NODES, ROLES, literal
from .surface import Call
from .values import (EXPRESSIONS, FREE_TEXT, GRAPH_ARGUMENTS, HOSTILE, HOSTILE_EXPRESSIONS, NUMBERS, ODD_WORDS,
                     QUERIES, QUERY_ARGUMENTS, REALS, RELATION_ARRAYS, SMALL, VECTOR_ARGUMENTS, VERSION_ARGUMENTS,
                     WORDS)

OPTIONAL_SHARE = 0.4
NEW_GRAPHS = 12
NEW_VECTORS = ("v0", "v1", "v2", "ones", "pr", "lbl")
# Each id type's source tables and the edge columns they have beyond src, dst.
SOURCES = {
    "bigint": ("public.nodes8", "public.edges8", ("rel", "w")),
    "integer": ("public.nodes4", "public.edges4", ("w",)),
    "text": ("public.nodes_text", "public.edges_text", ("w",)),
    "uuid": ("public.nodes_uuid", "public.edges_uuid", ()),
}


class Arguments:
    def __init__(self, rng, world, hostility):
        self.rng, self.world, self.hostility = rng, world, hostility

    def call(self, function):
        """A call of `function`: every required argument, and each optional
        one with probability `OPTIONAL_SHARE`."""
        self.graph, self.source, self.columns = None, None, ()
        self.graphs, self.queries = [], []
        positional = tuple(self._value(function, argument, i) for i, argument in enumerate(function.required))
        named = tuple((argument.name, self._value(function, argument, len(positional) + i))
                      for i, argument in enumerate(function.optional) if self.rng.random() < OPTIONAL_SHARE)
        return Call(function, positional, named, tuple(self.graphs), tuple(self.queries))

    def _value(self, function, argument, position):
        name, kind = argument.name, argument.type
        if name in GRAPH_ARGUMENTS or (name == "name" and position == 0):
            return self._graph(function, name)
        hostile = self.rng.random() < self.hostility
        if name in QUERY_ARGUMENTS:
            sql = self.rng.choice(QUERIES) if hostile else self._query(name)
            self.queries.append(sql)
            return sql
        if hostile:
            return self.rng.choice(HOSTILE.get(kind, ("NULL",)))
        return self._plain(name, kind)

    def _plain(self, name, kind):
        typed = {
            "anyelement": self._witness if name == "id_type" else self._node,
            "anyarray": self._nodes,
            '"any"': self._filter,
            "anycompatible": lambda: self.rng.choice(("NULL::float8", "NULL::float8[]", "NULL::bigint")),
            "warren.step[]": self._steps,
            "jsonb": lambda: self.rng.choice(("'[]'", "'[{\"rels\": [\"a\"], \"max_hops\": 2}]'",
                                              "'[{\"direction\": \"in\"}, {}]'")),
            "regrole": lambda: literal(self.rng.choice(ROLES)),
            "text[]": lambda: self.rng.choice(RELATION_ARRAYS),
            "boolean": lambda: self._flag(name),
            "integer": lambda: self._number(name),
            "bigint": lambda: self._version() if name in VERSION_ARGUMENTS else self._number(name),
            "double precision": lambda: self.rng.choice(REALS.get(name, SMALL)),
            "text": lambda: self._text(name),
        }.get(kind)
        return typed() if typed else "NULL"

    # Graphs and what is in them.

    def _graph(self, function, name):
        """An existing graph, mostly; for a graph to be made, mostly a new
        name; sometimes one that does not exist or a hostile name."""
        making = function.name == "create_graph" or name == "out_graph"
        roll = self.rng.random()
        if roll < self.hostility * 0.5:
            sql = self.rng.choice(HOSTILE["text"])
        elif (making and roll < 0.6) or not self.world.graphs or roll > 0.95:
            sql = literal(f"fz{self.rng.randrange(NEW_GRAPHS)}")
        else:
            sql = literal(self.rng.choice(self.world.graphs).name)
        graph = self.world.graph(sql[1:-1]) if sql.startswith("'") else None
        if graph is not None:
            self.graphs.append(graph.name)
        if self.graph is None or graph is not None:
            self.graph = graph
        return sql

    def _id_type(self):
        return self.graph.id_type if self.graph and self.graph.id_type in NODES else self.rng.choice(sorted(NODES))

    def _node(self):
        return self.rng.choice(NODES[self._id_type()])

    def _nodes(self):
        id_type = self._id_type()
        nodes = self.rng.sample(NODES[id_type], self.rng.randint(1, 3))
        return f"ARRAY[{', '.join(nodes)}]::{id_type}[]"

    def _filter(self):
        return self._nodes() if self.rng.random() < 0.7 else f"NULL::{self._id_type()}[]"

    def _witness(self):
        return f"NULL::{self._id_type()}"

    def _steps(self):
        steps = []
        for _ in range(self.rng.randint(1, 3)):
            low = self.rng.randint(0, 2)
            high = self.rng.choice(("NULL", str(low + self.rng.randint(0, 2))))
            steps.append(f"warren.s({self.rng.choice(RELATION_ARRAYS)}, {literal(self.rng.choice(WORDS['direction']))}, "
                         f"{self._filter()}, {low}, {high})")
        return f"ARRAY[{', '.join(steps)}]"

    def _version(self):
        versions = self.world.versions_of(self.graph.name) if self.graph else []
        if not versions or self.rng.random() < 0.3:
            return "NULL"
        return str(self.rng.choice(versions))

    def _vector(self):
        names = self.world.vectors_of(self.graph.name) if self.graph else []
        pool = names if names and self.rng.random() < 0.6 else NEW_VECTORS
        return self.rng.choice(pool)

    # Scalars.

    def _flag(self, name):
        column = {"weighted": "w", "has_relations": "rel"}.get(name)
        if column and self.source and self.rng.random() < 0.8:
            return "true" if column in self.columns else "false"
        return self.rng.choice(("true", "false"))

    def _number(self, name):
        return self.rng.choice(NUMBERS.get(name, SMALL))

    def _text(self, name):
        if name in VECTOR_ARGUMENTS:
            return literal(self._vector())
        if name == "expr":
            return self._expression()
        if name in WORDS:
            words = WORDS[name] if self.rng.random() < 0.9 else ODD_WORDS
            return literal(self.rng.choice(words))
        return self.rng.choice(FREE_TEXT)

    def _expression(self):
        vector, other = self._vector(), self._vector()
        if self.rng.random() < self.hostility:
            return self.rng.choice(HOSTILE_EXPRESSIONS).replace("{v}", vector)
        return literal(self.rng.choice(EXPRESSIONS).replace("{v}", vector).replace("{w}", other))

    # The caller's SQL.

    def _query(self, name):
        """A query over the source tables of the context's id type (or, for
        a new graph's, of a type chosen here), in the shape `name` takes."""
        if self.source is None:
            id_type = self.graph.id_type if self.graph and self.graph.id_type in SOURCES else None
            self.source = SOURCES[id_type or self.rng.choice(sorted(SOURCES))]
        nodes, edges, extra = self.source
        if name == "node_query":
            return literal(f"SELECT id FROM {nodes}")
        if name == "edge_query":
            self.columns = tuple(c for c in extra if self.rng.random() < 0.6)
            return literal(f"SELECT {', '.join(('src', 'dst', *self.columns))} FROM {edges}")
        if name == "pairs":
            return literal(f"SELECT src, dst FROM {edges} LIMIT {self.rng.choice((1, 20, 200))}")
        if name == "messages" and self.rng.random() < 0.5 and "w" in extra:
            return literal(f"SELECT src, w FROM {edges}")
        value = self.rng.choice(("1", "0.5", "pg_catalog.length(id::text)", "NULL", "-3"))
        return literal(f"SELECT id, ({value})::float8 FROM {nodes}")
