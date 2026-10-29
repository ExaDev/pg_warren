"""pg_warren added to `shared_preload_libraries` of a cluster that already
has graphs, and removed again (DESIGN.md §5.6, §13 0.9.0 "Lifecycle"), as
a managed service's parameter group changes it, with a restart. The
preload places the shared cache's registry and the activity table at
startup (on PG 15 and 16 the only way to have them); nothing persisted
depends on it, so the graphs read the same either way.

The check (`test/preload.py`) builds the replica check's graphs on a
cluster without the preload, then for each step (`STEPS`) changes the
setting, restarts, and checks that the catalog and every read are as
before the restart (`graph_state`), that the shared cache works (and
shares, preloaded), and that the graphs can be written."""

from . import server_log, shared_cache

STEPS = (
    ("preloaded", ("shared_preload_libraries = 'pg_warren'", f"warren.shared_cache_memory = '{shared_cache.CAP}'")),
    ("no longer preloaded", ("shared_preload_libraries = ''",)),
)

# What the server log may hold, by SQLSTATE: the stranger's reads
# (42501), and connections ended when the server restarts (57P01).
ALLOWED = {"42501", "57P01"}


def log_problems(text):
    return [f"server log: {line}" for line in server_log.problems(text, ALLOWED, server_log.CLEAN_SHUTDOWN)]
