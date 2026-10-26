"""What must hold once the pooler soak's clients have stopped (DESIGN.md
§13 M11): nothing a client left that a pooler's reset or a backend's
exit should have freed, and nothing in the catalog that leaked.

`after_clients`, with the poolers still up and their server backends
idle: the session pool's backends hold no transient vector (its
`DISCARD ALL` reset each one when its client left); the transaction
pool's are reported (no reset runs between transactions). `after_poolers`,
once the poolers have closed their server connections: no activity row
of a backend that has gone, no progress row, nothing pinned or mapped in
the shared cache, no staged version, no segment or chunk without its
version or vector, every version's segments adding up to its bytes, the
vectors the clients left (each client reuses its names, so at most a few
per client), a refresh of each graph keeping `retain_versions` and,
after `evict`, no shared entry left and no more DSM segments mapped than
before the soak."""

import time

from .fixture import GRAPHS
from .workload import PERSISTED_NAMES_PER_CLIENT

SETTLE_SECONDS = 15
RETAIN = {"g": 3, "t": 2}


def until(admin, sql, seconds=SETTLE_SECONDS):
    """Polls `sql`, a count, until it is 0 or `seconds` pass; its last value."""
    deadline = time.monotonic() + seconds
    while (value := admin.run(sql).value()) != "0" and time.monotonic() < deadline:
        time.sleep(0.2)
    return value


def after_clients(admin, roles):
    """Failures, and a report of the transient bytes the transaction pool's backends keep."""
    failures, report = [], {}
    for mode, role in roles.items():
        sql = (f"SELECT coalesce(sum(b.transient_bytes), 0) FROM warren.backends() b JOIN pg_stat_activity a "
               f"USING (pid) WHERE b.kind = 'backend' AND a.usename = '{role}'")
        if mode == "session":
            left = until(admin, f"SELECT count(*) FROM ({sql}) s(bytes) WHERE bytes > 0")
            if left != "0":
                failures.append(f"session pooling: {admin.run(sql).value()} bytes of transient vectors survive the "
                                "pooler's DISCARD ALL in backends whose clients have left")
        report[mode] = int(admin.run(sql).value())
    return failures, report


def after_poolers(admin, roles, clients, dsm_before, dsm_now):
    """Failures once the poolers have stopped; `dsm_now()` counts the DSM segments mapped now."""
    failures = []
    names = ",".join(roles.values())
    until(admin, f"SELECT count(*) FROM pg_stat_activity WHERE usename = ANY ('{{{names}}}')")
    checks = {
        "activity rows of backends that have gone": "SELECT count(*) FROM warren.backends() b WHERE kind = 'backend' "
        "AND NOT EXISTS (SELECT FROM pg_stat_activity a WHERE a.pid = b.pid)",
        "progress rows": "SELECT count(*) FROM warren.progress()",
        "shared entries pinned or mapped": "SELECT count(*) FROM warren.shared_cache() WHERE pins <> 0 OR mappers <> 0",
    }
    for what, sql in checks.items():
        if (left := until(admin, sql)) != "0":
            failures.append(f"{left} {what} left after the poolers stopped")
    failures += _catalog(admin, clients)
    for graph in GRAPHS:
        admin.run(f"SELECT warren.refresh('{graph}')").value()
        kept = admin.run(f"SELECT count(*) FROM warren.versions JOIN warren.graphs USING (graph_id) "
                         f"WHERE name = '{graph}' AND NOT staged").value()
        if int(kept) > RETAIN[graph]:
            failures.append(f"graph {graph} keeps {kept} versions after a refresh, "
                            f"beyond retain_versions {RETAIN[graph]}")
        admin.run(f"SELECT warren.evict('{graph}')").value()
    if (left := until(admin, "SELECT count(*) FROM warren.shared_cache() WHERE kind IN ('version', 'vector')")) != "0":
        failures.append(f"{left} shared entries left after evict")
    deadline = time.monotonic() + SETTLE_SECONDS
    while (mapped := dsm_now()) > dsm_before and time.monotonic() < deadline:
        time.sleep(0.2)
    if mapped > dsm_before:
        failures.append(f"{mapped} DSM segments mapped after evict, {dsm_before} before the soak")
    return failures


def _catalog(admin, clients):
    failures = []
    leaks = {
        "staged versions": "SELECT count(*) FROM warren.versions WHERE staged",
        "segments without their version": "SELECT count(*) FROM warren.segments s WHERE NOT EXISTS "
        "(SELECT FROM warren.versions v WHERE v.version_id = s.version_id)",
        "vector chunks without their vector": "SELECT count(*) FROM warren.vector_chunks c WHERE NOT EXISTS "
        "(SELECT FROM warren.vectors v WHERE v.vector_id = c.vector_id)",
        "versions whose segments do not add up to their bytes": "SELECT count(*) FROM warren.versions v WHERE bytes <> "
        "(SELECT coalesce(sum(length(data)), 0) FROM warren.segments s WHERE s.version_id = v.version_id)",
    }
    for what, sql in leaks.items():
        if (left := admin.run(sql).value()) != "0":
            failures.append(f"{left} {what}")
    vectors = int(admin.run("SELECT count(*) FROM warren.vectors").value())
    if vectors > clients * PERSISTED_NAMES_PER_CLIENT:
        failures.append(f"{vectors} persisted vectors left, more than the clients' names ({clients} clients)")
    admin.run("SELECT count(warren.drop_vector(g.name, v.name, v.version_id, true)) FROM warren.vectors v "
              "JOIN warren.graphs g USING (graph_id)").value()
    return failures
