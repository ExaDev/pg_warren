"""Deterministic draws in SQL, so a use case's data is generated inside the
database (no fixture files: at 10M nodes they would not fit the disk
beside the graph) and is the same on every run and plan: each draw is a
hash of a key and a salt, never `random()`."""

FUNCTIONS = (
    # Uniform in [0, 1): the top 53 bits of a 64-bit hash.
    "CREATE FUNCTION draw(key bigint, salt int) RETURNS float8 IMMUTABLE PARALLEL SAFE LANGUAGE sql "
    "RETURN ((hashint8extended(key, salt) >> 11) & 9007199254740991)::float8 / 9007199254740992",
    # Uniform in 0 .. n - 1.
    "CREATE FUNCTION pick(key bigint, salt int, n bigint) RETURNS bigint IMMUTABLE PARALLEL SAFE LANGUAGE sql "
    "RETURN floor(draw(key, salt) * n)::bigint",
    # Standard normal (Box-Muller over two draws).
    "CREATE FUNCTION gauss(key bigint, salt int) RETURNS float8 IMMUTABLE PARALLEL SAFE LANGUAGE sql "
    "RETURN sqrt(-2 * ln(1 - draw(key, salt))) * cos(2 * pi() * draw(key, salt + 1))",
    # A bijection of 0 .. n - 1 onto 1 .. n (n below the prime 2^31 - 1),
    # so ids do not reveal the structure a generator planted.
    "CREATE FUNCTION shuffle(idx bigint, n bigint) RETURNS bigint IMMUTABLE PARALLEL SAFE LANGUAGE sql "
    "RETURN 1 + (idx * 2147483647) % n",
    # The SQLSTATE `statement` raises, or NULL if it succeeds.
    """CREATE FUNCTION sqlstate_of(statement text) RETURNS text LANGUAGE plpgsql AS $$
BEGIN EXECUTE statement; RETURN NULL;
EXCEPTION WHEN OTHERS THEN RETURN SQLSTATE; END $$""",
)


def embedding(d, noise, key, centroid):
    """A width-`d` real[] expression: row `centroid` (a real[] expression)
    plus Gaussian noise of expected norm `noise`, drawn by `key`."""
    scale = noise / d ** 0.5
    return "ARRAY[" + ", ".join(f"{centroid}[{k + 1}] + {scale:.9g} * gauss({d} * ({key}) + {k}, 60)"
                                for k in range(d)) + "]::real[]"


def centroids(name, communities, d):
    """A table `name (c, centroid real[])` of unit-length community centroids."""
    return (f"CREATE TABLE {name} AS SELECT c, array_agg((x / norm)::real ORDER BY k) AS centroid FROM ("
            f"SELECT c, k, x, sqrt(sum(x * x) OVER (PARTITION BY c)) AS norm FROM ("
            f"SELECT c, k, gauss({d} * c + k, 70) AS x FROM generate_series(0, {communities}) c, "
            f"generate_series(0, {d - 1}) k) g) n GROUP BY c")
