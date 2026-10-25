"""The surface the fuzzer calls: every function in schema `warren`, as
`pg_proc` describes it (input arguments, how many have defaults, whether
it returns a set), and the statements that call one. Reading it from the
catalog means a new function is fuzzed as soon as it exists."""

import json
from dataclasses import dataclass, replace

# One row: a JSON array of every warren function but the trigger
# functions, with its input arguments (IN, INOUT and VARIADIC) in order.
SURFACE_SQL = """
SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
    'name', p.proname, 'defaults', p.pronargdefaults, 'set', p.proretset, 'strict', p.proisstrict,
    'result', pg_catalog.format_type(p.prorettype, NULL),
    'args', (SELECT pg_catalog.json_agg(pg_catalog.json_build_object(
                        'name', a.name, 'type', pg_catalog.format_type(a.type, NULL)) ORDER BY a.n)
             FROM ROWS FROM (pg_catalog.unnest(coalesce(p.proallargtypes, p.proargtypes::pg_catalog.oid[])),
                             pg_catalog.unnest(p.proargmodes), pg_catalog.unnest(p.proargnames))
                  WITH ORDINALITY AS a(type, mode, name, n)
             WHERE coalesce(a.mode, 'i') IN ('i', 'b', 'v')))
    ORDER BY p.proname, p.oid)
FROM pg_catalog.pg_proc p
WHERE p.pronamespace = 'warren'::pg_catalog.regnamespace AND p.prorettype <> 'trigger'::pg_catalog.regtype
"""


@dataclass(frozen=True)
class Argument:
    name: str
    type: str


@dataclass(frozen=True)
class Function:
    name: str
    arguments: tuple
    defaults: int
    returns_set: bool
    result: str
    strict: bool = False

    @property
    def required(self):
        return self.arguments[:len(self.arguments) - self.defaults]

    @property
    def optional(self):
        return self.arguments[len(self.arguments) - self.defaults:]


def parse(text):
    """The functions `SURFACE_SQL`'s JSON describes."""
    return tuple(Function(row["name"], tuple(Argument(a["name"], a["type"]) for a in row["args"] or ()),
                          row["defaults"], row["set"], row["result"], row["strict"]) for row in json.loads(text))


def ident(name):
    """`name` as a quoted SQL identifier."""
    return '"' + name.replace('"', '""') + '"'


@dataclass(frozen=True)
class Call:
    """A call of `function`: its required arguments' SQL by position, and
    some optional ones' by name; the existing graphs it names, and the SQL
    text it hands the function to run (for `verdict`)."""
    function: Function
    positional: tuple
    named: tuple = ()
    graphs: tuple = ()
    queries: tuple = ()

    def expression(self):
        arguments = [*self.positional, *(f"{ident(name)} => {sql}" for name, sql in self.named)]
        return f"warren.{ident(self.function.name)}({', '.join(arguments)})"

    def statement(self, form, limit=1):
        """The statement that makes this call and returns one short row:
        `value` measures a scalar result as text; `rows` counts a set's
        rows, run to completion in FROM; `first` counts the first `limit`
        rows of a set returned one at a time from the select list."""
        if form == "value":
            return f"SELECT pg_catalog.length(CAST({self.expression()} AS text))"
        if form == "rows":
            return f"SELECT pg_catalog.count(*) FROM {self.expression()} AS r"
        if form == "first":
            return f"SELECT pg_catalog.count(*) FROM (SELECT {self.expression()} LIMIT {limit}) AS r"
        raise ValueError(form)

    def skipped(self):
        """Whether Postgres returns NULL without calling the function: it is
        STRICT and an argument is a NULL literal."""
        return self.function.strict and any(sql.startswith("NULL") for sql in
                                            (*self.positional, *(sql for _, sql in self.named)))

    def argument(self, name):
        """The SQL given for argument `name`, or None if it is not given."""
        position = next((i for i, a in enumerate(self.function.arguments) if a.name == name), None)
        if position is not None and position < len(self.positional):
            return self.positional[position]
        return dict(self.named).get(name)

    def without(self, name):
        """This call without the named argument `name`."""
        return replace(self, named=tuple((n, sql) for n, sql in self.named if n != name))
