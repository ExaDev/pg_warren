"""The pooler soak's database: its roles, source tables and graphs.

Each pool mode connects as its own login role (`role(mode)`), so the
sampler tells the pools' server backends apart by user; both are members
of `OWNER`, which owns the source tables and the graphs, so either pool
may read, write vectors and refresh. Graph `g` has `bigint` ids and
weights and keeps 3 versions; graph `t` names the same nodes by `text`
ids and keeps 2. The source is `nodes` nodes, each with `OUT_DEGREE`
edges, and an edit (`edit_sql`) replaces one node's edges and removes or
brings back a node above n/2, so every refresh makes a slightly different
version."""

DATABASE = "soak"
OWNER = "soak_owner"
GRAPHS = ("g", "t")
OUT_DEGREE = 5
MULTIPLIERS = (1, 7919, 104729, 1299709, 15485863)


def role(mode):
    """The login role of pool mode `mode`'s clients."""
    return f"soak_{mode}"


def setup_sql(nodes, modes):
    """The superuser's statements that make the roles, the source and both
    graphs, each with a first version."""
    logins = "".join(f"CREATE ROLE {role(mode)} LOGIN IN ROLE {OWNER};\n" for mode in modes)
    edges = " UNION ALL ".join(f"SELECT k, (k * {m}::bigint) % {nodes} + 1, 1 + (k * {m}::bigint) % 9 "
                               f"FROM generate_series(1, {nodes}) k" for m in MULTIPLIERS[:OUT_DEGREE])
    return [f"CREATE ROLE {OWNER} NOLOGIN;\n{logins}GRANT CREATE ON SCHEMA public TO {OWNER}",
            f"SET ROLE {OWNER}",
            "CREATE TABLE public.nodes (id bigint PRIMARY KEY)",
            "CREATE TABLE public.edges (src bigint NOT NULL, dst bigint NOT NULL, w float8 NOT NULL)",
            "CREATE INDEX ON public.edges (src)",
            f"INSERT INTO public.nodes SELECT generate_series(1, {nodes})",
            f"INSERT INTO public.edges SELECT * FROM ({edges}) e (src, dst, w) WHERE src <> dst",
            "SELECT warren.create_graph('g', 'SELECT id FROM public.nodes', 'SELECT src, dst, w FROM public.edges', "
            "weighted := true, retain_versions := 3)",
            "SELECT warren.create_graph('t', $$SELECT 'n' || id FROM public.nodes$$, "
            "$$SELECT 'n' || src, 'n' || dst FROM public.edges$$, retain_versions := 2)",
            "SELECT warren.refresh('g')", "SELECT warren.refresh('t')", "RESET ROLE"]


def edit_sql(rng, nodes):
    """One edit of the source: a node's edges replaced, and a node above
    n/2 removed or brought back."""
    node = rng.randint(1, nodes)
    targets = sorted({rng.randint(1, nodes) for _ in range(OUT_DEGREE)} - {node})
    values = ", ".join(f"({node}, {target}, {rng.randint(1, 9)})" for target in targets)
    toggled = rng.randint(nodes // 2 + 1, nodes)
    return (f"DELETE FROM public.edges WHERE src = {node}; INSERT INTO public.edges VALUES {values}; "
            f"WITH gone AS (DELETE FROM public.nodes WHERE id = {toggled} RETURNING id) "
            f"INSERT INTO public.nodes SELECT {toggled} WHERE NOT EXISTS (SELECT FROM gone)")
