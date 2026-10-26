"""The shared-cache modes of the SQL suite (DESIGN.md §5.6): `on`, the
default, adds nothing, so the cluster has the shipped default cap (4GB,
§14 item 32), which shares on PG 17 and 18 (the registry is a named DSM
segment) and loads privately on PG 15 and 16, which need the preload;
`preload`, pg_warren in `shared_preload_libraries` and a cap of its own,
so every major has the registry (after pg_stat_statements, so pg_warren's
hooks run with another's before them, whose ERRORs they must pass on); and `off`, the cap at `0`, as 0.4.0.
Tests of the shared cache read which applies from the server's
settings."""

CAP = "256MB"

MODES = {
    "on": (),
    "preload": ("shared_preload_libraries = 'pg_stat_statements, pg_warren'", f"warren.shared_cache_memory = '{CAP}'"),
    "off": ("warren.shared_cache_memory = 0",),
}

DEFAULT_MODE = "on"

# Whether a cluster shares versions: a cap is set and the registry exists
# (PG 17 and later, or pg_warren preloaded), as include/shared_cache.sql's
# :shared reads it.
SHARES = ("SELECT coalesce(current_setting('warren.shared_cache_memory', true), '0') <> '0' "
          "AND (current_setting('server_version_num')::int >= 170000 "
          "OR current_setting('shared_preload_libraries') ~ 'pg_warren')")


def settings(mode):
    """The postgresql.conf lines of `mode`."""
    return MODES[mode]
