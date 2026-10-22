"""The shared-cache modes of the SQL suite (DESIGN.md §5.6): `off`, the
default, as 0.4.0 (no cap set); `preload`, pg_warren in
`shared_preload_libraries` and a cap, so every major has the registry; and
`on`, the cap alone, which shares on PG 17 and 18 (the registry is a named
DSM segment) and loads privately, with a NOTICE, on PG 15 and 16. Tests of
the shared cache read which applies from the server's settings."""

CAP = "256MB"

MODES = {
    "off": (),
    "preload": ("shared_preload_libraries = 'pg_warren'", f"warren.shared_cache_memory = '{CAP}'"),
    "on": (f"warren.shared_cache_memory = '{CAP}'",),
}


def settings(mode):
    """The postgresql.conf lines of `mode`."""
    return MODES[mode]
