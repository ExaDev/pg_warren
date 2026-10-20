#!/usr/bin/env python3
"""The 100M-edge soak (DESIGN.md §12 "Performance", §13 M7): for each case,
build a random directed graph of 10M nodes and 100M edges generated inside
a scratch cluster (no fixture files) and measure every §12 row at the
default limits, with each backend's real peak memory (`/proc/<pid>/status`
VmHWM, reset through `clear_refs`, and the peak of RssAnon polled every
20 ms). Writes `<results>/<case>.json`, with the hardware, the cluster
settings, the provenance (argv, the harness's and the extension's
revisions, the build profile and the installed library's sha256) and a
verdict per §12 target: a stage that fails, or that a lost backend never
reached, is a miss carrying its error.

    bench/soak.py a b-merged b-unmerged --pg-config /path/to/pg_config --parent /var/tmp \
        --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
    bench/soak.py a --scale 1000 --no-eigen-steps --extension-revision … --extension-profile debug   # a quick check

Cases: `a` is unweighted with one relation; `b-merged` and `b-unmerged` are
weighted with five relations, with and without the stored merged CSR. Each
case runs in its own database, dropped before the next. The edges are
G(n, m) with replacement from a seeded `random()`: case `a` reads them
from an unlogged `(src, dst)` table (which the recursive CTE baseline
needs) and also builds once from the generator query directly, to compare;
the `b` cases read the generator query, so they need no table on disk.
Relations and weights are derived from the endpoints. A full-scale case
whose first version does not have the 47 deduplicated edges the committed
runs had (the edges depend on the plan evaluating the seeding subquery
once) is aborted. A case whose version
(or the graph and `pagerank`'s scratch) does not fit
`warren.max_cache_memory` is measured again with the cap raised. A second
version, without the 0.1% of edges whose source is 7 mod 1000, is built
for `diff` and `distance` only if the disk keeps `--disk-floor` free."""

import argparse
import datetime
import hashlib
import json
import os
import platform
import queue
import re
import resource
import shutil
import statistics
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, replace
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "test"))

from harness.cluster import Cluster  # noqa: E402
from matrix import TIMER  # noqa: E402

GIB = 1 << 30
GRAPH = "soak"
DEFAULT_CAP = 4 * GIB  # warren.max_cache_memory, max_build_memory and max_compute_memory
PAGERANK_SCRATCH_VECTORS = 11  # full-length f64 transient vectors (DESIGN.md §5.5)
# The probe that reads a version's cached size from the unfit notice: a
# tiny cap, and compute memory lifted so that a version past both still
# loads (DESIGN.md §5.5: a load past the two together is refused).
COLD_LOAD_PROBE_SETTINGS = ("SET warren.max_cache_memory = '64kB'",
                            "SET warren.max_compute_memory = '2147483647kB'")
# The rows of warren.segments this transaction has read (an index scan
# counts its heap fetches on the index), in a subquery `{when}` correlates
# so it runs after what it names.
SEGMENT_READS = ("(SELECT pg_stat_get_xact_tuples_returned('warren.segments'::regclass)"
                 " + pg_stat_get_xact_tuples_fetched('warren.segments'::regclass)"
                 " + sum(pg_stat_get_xact_tuples_fetched(indexrelid))::bigint"
                 " FROM pg_index WHERE indrelid = 'warren.segments'::regclass AND {when} IS NOT NULL)")
SETTINGS = (
    "shared_buffers = '1GB'", "work_mem = '64MB'", "maintenance_work_mem = '1GB'",
    "max_wal_size = '1GB'", "min_wal_size = '256MB'", "wal_level = minimal", "max_wal_senders = 0",
    "checkpoint_timeout = '15min'", "autovacuum = off",
)
MEMORY_FIELDS = ("VmHWM", "VmRSS", "RssAnon", "RssFile", "RssShmem")
SETTINGS_SQL = (
    "SELECT name || '=' || current_setting(name) FROM pg_settings WHERE name IN "
    "('shared_buffers', 'work_mem', 'maintenance_work_mem', 'max_wal_size', 'wal_level', 'fsync', "
    "'synchronous_commit', 'full_page_writes', 'jit', 'max_parallel_workers_per_gather', "
    "'statement_timeout', 'warren.max_cache_memory', 'warren.max_build_memory', 'warren.max_compute_memory', "
    "'warren.threads') ORDER BY 1")


@dataclass(frozen=True)
class Case:
    name: str
    weighted: bool
    relations: int
    store_merged: bool
    table: bool  # read the edges from a table (else from the generator query)
    nodes: int = 10_000_000
    edges: int = 100_000_000

    def scaled(self, factor):
        return replace(self, nodes=self.nodes // factor, edges=self.edges // factor)


CASES = {
    "a": Case("a", weighted=False, relations=1, store_merged=True, table=True),
    "b-merged": Case("b-merged", weighted=True, relations=5, store_merged=True, table=False),
    "b-unmerged": Case("b-unmerged", weighted=True, relations=5, store_merged=False, table=False),
}
SEED = 0.25
# The deduplicated edges of every full-scale case with SEED (the parallel
# edges the generator draws), recorded by the committed runs on PG 16: a
# plan that evaluates the seeding subquery differently draws other edges.
DEDUPLICATED_EDGES = 47
EIGEN_TOL = 1e-6
REMOVED, KEPT = "src % 1000 = 7", "src % 1000 <> 7"  # the edges the second version leaves out, and keeps


def generator_sql(case, seed):
    """Uniform random directed edges, G(n, m) with replacement, seeded in
    the query itself so every scan draws the same edges."""
    return (f"SELECT 1 + floor(random() * {case.nodes})::int4 AS src, 1 + floor(random() * {case.nodes})::int4 AS dst "
            f"FROM (SELECT setseed({seed}) OFFSET 0) seeded CROSS JOIN generate_series(1, {case.edges})")


def edge_query(case, seed, table=None):
    """The graph's edge query: endpoints as `bigint` (an `int8` id map), the
    relation and weight of a multi-relation case derived from them."""
    columns = "src::bigint, dst::bigint"
    if case.relations > 1 or case.weighted:
        columns += f", 'r' || ((src # dst) % {case.relations}), (1 + (src + 3 * dst) % 9)::float8"
    if case.table if table is None else table:
        return f"SELECT {columns} FROM soak_edges"
    return (f"SELECT {columns} FROM ({generator_sql(case, seed)}) e "
            f"WHERE {KEPT} OR NOT EXISTS (SELECT FROM soak_removed)")


def create_graph_sql(case, seed, name=GRAPH, table=None):
    multi = case.relations > 1
    return (f"SELECT warren.create_graph('{name}', 'SELECT id FROM generate_series(1::bigint, {case.nodes}) id', "
            f"$q${edge_query(case, seed, table)}$q$, weighted := {str(case.weighted).lower()}, "
            f"has_relations := {str(multi).lower()}, store_merged := {str(case.store_merged).lower()})")


def table_sql(case, seed):
    """The edge table of a table case, and the marker table of the second version."""
    statements = ["CREATE UNLOGGED TABLE soak_removed ()"]
    if case.table:
        statements += [
            "CREATE UNLOGGED TABLE soak_edges (src int4 NOT NULL, dst int4 NOT NULL) WITH (autovacuum_enabled = false)",
            f"INSERT INTO soak_edges {generator_sql(case, seed)}",
            "ANALYZE soak_edges",
        ]
    return statements


def remove_sql(case):
    """Leaves out the 0.1% of edges whose source is 7 mod 1000, for the second version."""
    if case.table:
        return [f"DELETE FROM soak_edges WHERE {REMOVED}", "ANALYZE soak_edges"]
    return ["INSERT INTO soak_removed DEFAULT VALUES"]


def segment_estimate(case):
    """The stored bytes of a version (DESIGN.md §5.3): per direction, each
    CSR's offsets and each copy of the edges' targets (u32) and weights; the
    merged CSR is a second copy when stored with several relations."""
    csrs = case.relations + (1 if case.relations > 1 and case.store_merged else 0)
    copies = 2 if case.relations > 1 and case.store_merged else 1
    per_edge = 4 + (8 if case.weighted else 0)
    direction = 8 * (case.nodes + 1) * csrs + per_edge * case.edges * copies
    return 2 * direction + 16 * case.nodes + case.nodes // 8


def memory_status(text):
    """The memory fields of a `/proc/<pid>/status`, in bytes."""
    fields = {}
    for line in text.splitlines():
        key, _, value = line.partition(":")
        if key in MEMORY_FIELDS:
            fields[key] = int(value.split()[0]) * 1024
    return fields


UNFIT = re.compile(r"version (\d+) \((\d+) bytes\) cannot be kept within warren\.max_cache_memory")


def cached_bytes(notices, version):
    """The bytes the cache counts for `version`, from the notice a load
    under a tiny `warren.max_cache_memory` raises (DESIGN.md §5.5)."""
    for notice in notices:
        found = UNFIT.search(notice)
        if found and int(found.group(1)) == version:
            return int(found.group(2))
    return None


SEVERITY = re.compile(r"^(?:psql:[^:]*:\d+: )?(NOTICE|WARNING|ERROR|DETAIL|HINT|CONTEXT|FATAL):\s")


def messages(lines):
    """psql's stderr lines by severity; DETAIL, HINT and CONTEXT follow an error."""
    split = {"notices": [], "warnings": [], "errors": []}
    for line in lines:
        found = SEVERITY.match(line)
        if not found:
            continue
        text = line[found.start(1):]
        kind = {"NOTICE": "notices", "WARNING": "warnings"}.get(found.group(1), "errors")
        split[kind].append(text)
    return split


def disk_allows(free, needed, floor):
    return free - needed >= floor


def with_segment_reads(call):
    """`SELECT <call>` as one statement that also returns the rows of
    `warren.segments` it read, as `reads|result`. A load's reads, from the
    pass's first (cold) `prewarm`, turn another call's into a load count."""
    return (f"WITH before AS MATERIALIZED (SELECT {SEGMENT_READS.format(when='1')} AS reads), "
            f"run AS MATERIALIZED (SELECT {call} AS result FROM before WHERE before.reads IS NOT NULL), "
            f"after AS MATERIALIZED (SELECT {SEGMENT_READS.format(when='run.result')} AS reads FROM run) "
            f"SELECT after.reads - before.reads, run.result FROM before, run, after")


def pagerank_sql(graph):
    """`pagerank`, 20 iterations, counting its segment reads."""
    return with_segment_reads(f"warren.pagerank('{graph}', 'pr', tol := 0, max_iter := 20)")


def prewarm_sql(graph):
    """A load of `graph`'s current version, counting its segment reads."""
    return with_segment_reads(f"warren.prewarm('{graph}')")


def cap_bytes(kb):
    """The first pass's `warren.max_cache_memory` in bytes: `kb`, or the default."""
    return DEFAULT_CAP if kb is None else kb * 1024


def pagerank_fits(cached, id_space, cap):
    """Whether the version and `pagerank`'s transient scratch fit the cap together."""
    return cached + PAGERANK_SCRATCH_VECTORS * 8 * id_space <= cap


def size(value):
    for unit, scale in (("GB", 1e9), ("MB", 1e6), ("kB", 1e3)):
        if value >= scale:
            return f"{value / scale:.2f} {unit}"
    return f"{value} B"


def _seconds(value):
    return f"{value:.1f} s"


TARGETS = (
    ("bfs depth 6 vs recursive CTE", "≥ 20× faster", ("bfs_seconds", "bfs_cte_seconds"),
     lambda m: m["bfs_cte_seconds"] / m["bfs_seconds"] >= 20,
     lambda m: f"{m['bfs_cte_seconds'] / m['bfs_seconds']:.1f}×"),
    ("reachable_set, full component", "> 50M edges/s", ("reachable_edges_per_second",),
     lambda m: m["reachable_edges_per_second"] > 50e6,
     lambda m: f"{m['reachable_edges_per_second'] / 1e6:.0f}M edges/s"),
    ("components()", "< 10 s", ("components_seconds",),
     lambda m: m["components_seconds"] < 10, lambda m: _seconds(m["components_seconds"])),
    ("pagerank, 20 iterations", "< 30 s (8 threads)", ("pagerank_seconds",),
     lambda m: m["pagerank_seconds"] < 30, lambda m: _seconds(m["pagerank_seconds"])),
    ("eigen, k = 20, normalised Laplacian", "< 10 min, converged", ("eigen_seconds", "eigen_converged", "eigen_residual"),
     lambda m: m["eigen_seconds"] < 600 and m["eigen_converged"],
     lambda m: f"{_seconds(m['eigen_seconds'])}, {'converged' if m['eigen_converged'] else 'not converged'}, "
               f"residual {m['eigen_residual']:.1e}"),
    ("refresh() incl. reverse CSR", "< 8 min, < 4 GiB RAM", ("refresh_seconds", "refresh_peak_bytes"),
     lambda m: m["refresh_seconds"] < 480 and m["refresh_peak_bytes"] < 4 * GIB,
     lambda m: f"{_seconds(m['refresh_seconds'])}, peak {size(m['refresh_peak_bytes'])}"),
    ("diff between two versions", "< 30 s", ("diff_seconds",),
     lambda m: m["diff_seconds"] < 30, lambda m: _seconds(m["diff_seconds"])),
    ("cold load into backend", "< 15 s", ("cold_load_seconds",),
     lambda m: m["cold_load_seconds"] < 15, lambda m: _seconds(m["cold_load_seconds"])),
    ("memory: graph + pagerank scratch within max_cache_memory", "fits 4 GiB",
     ("cached_bytes", "id_space", "max_cache_memory"),
     lambda m: pagerank_fits(m["cached_bytes"], m["id_space"], m["max_cache_memory"]),
     lambda m: f"{size(m['cached_bytes'])} + {size(PAGERANK_SCRATCH_VECTORS * 8 * m['id_space'])} scratch"),
)


STOPPED = "canceling statement"  # a cancel or statement_timeout: the time is a lower bound
# warren.eigen's WARNING when its test (|β·s| ≤ tol·max|θ|, DESIGN.md §6.5)
# fails within max_iter. The explicit residual it returns floors near
# 1e-7·‖M‖ in f32, so `residual ≤ tol` is not that test.
UNCONVERGED = "eigen did not converge"


def verdicts(measured):
    """Each §12 target the measurements cover, met or not. A stage that
    failed, or was planned but never reached, is a miss carrying its error;
    a stage stopped before it finished is a miss, its time a lower bound."""
    found = []
    for name, target, keys, met, show in TARGETS:
        failed = next((measured[f"{k}_failed"] for k in keys if f"{k}_failed" in measured), None)
        stopped = [k for k in keys if measured.get(f"{k}_stopped")]
        if failed is not None:
            found.append({"workload": name, "target": target, "measured": f"failed: {failed}", "error": failed,
                          "met": False})
        elif stopped:
            found.append({"workload": name, "target": target, "met": False,
                          "measured": f"≥ {_seconds(measured[stopped[0]])} (stopped)"})
        elif all(k in measured for k in keys):
            found.append({"workload": name, "target": target, "measured": show(measured), "met": bool(met(measured))})
    return found


class BackendLost(RuntimeError):
    """The backend (or psql) went away mid-statement, e.g. killed for memory."""


class GeneratorDrift(RuntimeError):
    """The generator drew other edges than the committed runs did."""


def check_edges(case, stats):
    """Fails unless a full-scale case's first version has the deduplicated
    edges every committed run had (scaled cases have no recorded value)."""
    if case != CASES[case.name]:
        return
    found, edges = stats["deduplicated_edges"], stats["edge_count"]
    if (found, edges) != (DEDUPLICATED_EDGES, case.edges - DEDUPLICATED_EDGES):
        raise GeneratorDrift(f"version 1 has {found} deduplicated edges, {edges} edges; expected "
                             f"{DEDUPLICATED_EDGES} and {case.edges - DEDUPLICATED_EDGES}")


def planned_stages(case, options):
    """The stages a verdict reads that `case` runs with `options`, unless
    skipped for disk or lost with the backend."""
    planned = {"refresh", "cold_load_probe", "default: prewarm", "default: bfs depth 6", "default: reachable_set",
               "default: components", "default: pagerank"}
    if options.eigen:
        planned.add("default: eigen")
    if case.table and options.cte:
        planned.add("bfs_cte")
    if "second-version" not in options.skip:
        planned.add("diff")
    return sorted(planned)


class Session:
    """One backend, driven through a psql coprocess, so a sequence of
    statements shares the backend's cache and its memory can be watched."""

    MARK = "@@soak-done@@"

    def __init__(self, cluster, database):
        command = [cluster.client("psql"), "-X", "-q", "-A", "-t", "-v", "ON_ERROR_STOP=0",
                   *cluster.connection(database)]
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.PIPE, text=True, bufsize=1)
        self.stderr = queue.Queue()
        threading.Thread(target=self._drain, daemon=True).start()
        self.pid = int(self.run("SELECT pg_backend_pid()")["rows"][0])

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.process.stdin.close()
        self.process.wait()

    def _drain(self):
        for line in self.process.stderr:
            self.stderr.put(line.rstrip("\n"))

    def run(self, sql):
        """Runs `sql`: its output rows, messages and wall seconds."""
        started = time.perf_counter()
        self.process.stdin.write(f"{sql.rstrip().rstrip(';')};\n\\echo {self.MARK}\n\\warn {self.MARK}\n")
        self.process.stdin.flush()
        rows, marked = [], False
        for line in self.process.stdout:
            if line.rstrip("\n") == self.MARK:
                marked = True
                break
            rows.append(line.rstrip("\n"))
        if not marked:
            raise BackendLost(f"psql ended during: {sql[:200]}")
        seconds = time.perf_counter() - started
        lines = []
        while (line := self.stderr.get()) != self.MARK:
            lines.append(line)
        return {"rows": rows, "seconds": seconds, **messages(lines)}

    def status(self):
        return memory_status(Path(f"/proc/{self.pid}/status").read_text())

    def measure(self, sql, repeat=1):
        """Runs `sql` `repeat` times with the backend's peak memory reset first."""
        Path(f"/proc/{self.pid}/clear_refs").write_text("5")
        before = self.status()
        peak_anon, done = [before["RssAnon"]], threading.Event()

        def poll():
            while not done.wait(0.02):
                try:
                    peak_anon[0] = max(peak_anon[0], self.status()["RssAnon"])
                except (FileNotFoundError, KeyError):
                    return

        poller = threading.Thread(target=poll, daemon=True)
        poller.start()
        runs = [self.run(sql) for _ in range(repeat)]
        done.set()
        poller.join()
        after = self.status()
        last = runs[-1]
        result = {"sql": sql, "seconds": [round(r["seconds"], 4) for r in runs],
                  "median_seconds": round(statistics.median(r["seconds"] for r in runs), 4),
                  "rows": last["rows"][:5], "first_rows": [r["rows"][0] if r["rows"] else None for r in runs],
                  "notices": last["notices"][:5], "warnings": last["warnings"][:5],
                  "rss_anon_before": before["RssAnon"], "peak_rss_anon": max(peak_anon[0], after["RssAnon"]),
                  "vm_hwm": after["VmHWM"], "rss_anon_after": after["RssAnon"]}
        errors = [e for r in runs for e in r["errors"]]
        if errors:
            result["errors"] = errors[:5]
        print(f"    {sql[:90]}: {result['median_seconds']:.3f} s, peak anon {size(result['peak_rss_anon'])}, "
              f"HWM {size(result['vm_hwm'])}" + (f", ERROR {errors[0]}" if errors else ""), flush=True)
        return result


def timed(sql):
    """`sql` timed inside the backend (the bench harness's timer), for
    latencies too short for a client round trip."""
    return f"SELECT bench_seconds($soak${sql}$soak$)"


def server_seconds(stage):
    if "errors" not in stage:
        stage["server_seconds"] = [float(row) for row in stage["first_rows"]]
        stage["server_median_seconds"] = statistics.median(stage["server_seconds"])
    return stage


def free_disk(path):
    return shutil.disk_usage(path).free


def hardware():
    model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                  if line.startswith("model name")), platform.processor())
    memory = memory_total()
    return {"cpus": os.cpu_count(), "model": model, "memory_bytes": memory, "kernel": platform.release(),
            "load_average": os.getloadavg()}


def memory_total():
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            return int(line.split()[1]) * 1024
    return None


class Soak:
    def __init__(self, cluster, options, provenance):
        self.cluster, self.options, self.provenance = cluster, options, provenance

    def session(self):
        return Session(self.cluster, "soak")

    def stage(self, result, name, measured):
        result["stages"][name] = measured
        return measured

    def run(self, case):
        admin = self.cluster.database()
        admin.execute("DROP DATABASE IF EXISTS soak")
        admin.execute("CREATE DATABASE soak")
        result = {"provenance": self.provenance, "case": case.name, "nodes": case.nodes, "edges": case.edges, "weighted": case.weighted,
                  "relations": case.relations, "store_merged": case.store_merged, "threads": self.options.threads,
                  "eigen_max_iter": self.options.eigen_max_iter,
                  "statement_timeout": self.options.statement_timeout, "skip": self.options.skip, "planned_stages": planned_stages(case, self.options),
                  "hardware": hardware(), "started_at": _now(), "stages": {}, "skipped": {}}
        try:
            self._run(case, result)
        except (BackendLost, GeneratorDrift) as error:
            result["aborted"] = str(error)
            print(f"  ABORTED: {error}", flush=True)
        finally:
            admin.execute("DROP DATABASE IF EXISTS soak")
            result["finished_at"], result["free_disk_after"] = _now(), free_disk(self.options.parent)
            result["load_average_after"] = os.getloadavg()
        result["measured"] = summary(result)
        result["verdicts"] = verdicts(result["measured"])
        return result

    def _run(self, case, result):
        with self.session() as s:
            s.run("CREATE EXTENSION pg_warren")
            s.run(TIMER)
            result["pg_warren"] = s.run("SELECT warren.version()")["rows"][0]
            result["postgres"] = s.run("SELECT current_setting('server_version')")["rows"][0]
            needed = segment_estimate(case) + (40 * case.edges if case.table else 0) + GIB
            free = free_disk(self.options.parent)
            if not disk_allows(free, needed, self.options.disk_floor):
                result["skipped"]["case"] = f"disk: {size(free)} free, the case needs about {size(needed)}"
                return
            for i, sql in enumerate(table_sql(case, SEED)):
                self.stage(result, f"table {i}: {sql.split()[0].lower()}", s.measure(sql))
            if case.table:
                result["edge_table_bytes"] = int(s.run("SELECT pg_table_size('soak_edges')")["rows"][0])
                if self.options.cte:
                    self._cte_baseline(s, result)
        with self.session() as s:
            self.stage(result, "create_graph", s.measure(create_graph_sql(case, SEED)))
            self.stage(result, "refresh", s.measure(f"SELECT warren.refresh('{GRAPH}')"))
        if case.table:
            self._generated_refresh(case, result)
        v1 = self._version_stats(result, "version_1")
        check_edges(case, v1)
        with self.session() as s:
            for setting in COLD_LOAD_PROBE_SETTINGS:
                s.run(setting)
            probe = self.stage(result, "cold_load_probe", s.measure(f"SELECT warren.prewarm('{GRAPH}')"))
            result["cached_bytes"] = cached_bytes(probe["notices"], v1["version"])
        result["cache_memory_bytes"] = cap_bytes(self.options.cache_memory_kb)
        self._queries(result, "default", cap=self.first_cap())
        fits = result["cached_bytes"] is not None and pagerank_fits(result["cached_bytes"], v1["id_space"],
                                                                    result["cache_memory_bytes"])
        if not fits:
            self._queries(result, "raised", cap=self.options.raised_cache_memory)
        if self.options.eigen and self.options.eigen_steps and case.name == "a":
            self._eigen_steps(case, result)
        if "second-version" in self.options.skip:
            result["skipped"]["second_version"] = "--skip second-version"
        else:
            self._second_version(case, result, v1)

    def _generated_refresh(self, case, result):
        """The same build from the generator query, which the `b` cases
        read, to compare with the build from the table."""
        name = f"{GRAPH}_generated"
        with self.session() as s:
            s.run(create_graph_sql(case, SEED, name=name, table=False))
            self.stage(result, "refresh from the generator", s.measure(f"SELECT warren.refresh('{name}')"))
            s.run(f"SELECT warren.drop_graph('{name}')")
            s.run("VACUUM warren.segments")

    def _cte_baseline(self, s, result):
        if not disk_allows(free_disk(self.options.parent), result["edge_table_bytes"] // 2, self.options.disk_floor):
            result["skipped"]["bfs_cte"] = "disk: the (src) index would take the disk below the floor"
            return
        self.stage(result, "cte: index", s.measure("CREATE INDEX soak_edges_src ON soak_edges (src)"))
        cte = ("WITH RECURSIVE r (id, depth) AS (SELECT 1::int4, 0 UNION SELECT e.dst, r.depth + 1 FROM r "
               "JOIN soak_edges e ON e.src = r.id WHERE r.depth < 6) SELECT count(DISTINCT id) FROM r")
        self.stage(result, "bfs_cte", server_seconds(s.measure(timed(cte), 1)))
        self.stage(result, "bfs_cte count", s.measure(cte))
        s.run("DROP INDEX soak_edges_src")

    def _version_stats(self, result, name):
        with self.session() as s:
            info = json.loads(s.run(f"SELECT warren.info('{GRAPH}')")["rows"][0])
        stats = info["versions"][-1]
        result[name] = stats
        return stats

    def first_cap(self):
        """The first pass's `warren.max_cache_memory` setting, or None for the default."""
        kb = self.options.cache_memory_kb
        return None if kb is None else f"{kb}kB"

    def _queries(self, result, label, cap):
        """The read workloads in one backend, from a cold cache. A version
        that cannot fit the cap reloads on every statement, so latencies repeat less."""
        reloads = label == "default" and (result["cached_bytes"] or 0) > cap_bytes(self.options.cache_memory_kb)
        with self.session() as s:
            s.run(f"SET warren.threads = {self.options.threads}")
            if self.options.statement_timeout:
                s.run(f"SET statement_timeout = '{self.options.statement_timeout}'")
            if cap:
                s.run(f"SET warren.max_cache_memory = '{cap}'")

            def stage(name, sql, repeat=1, server=False):
                measured = s.measure(timed(sql) if server else sql, repeat)
                return self.stage(result, f"{label}: {name}", server_seconds(measured) if server else measured)

            stage("prewarm", prewarm_sql(GRAPH))
            # After the SETs, and after a call loaded the library: before
            # that pg_settings does not list warren.*.
            result.setdefault("settings", {})[label] = s.run(SETTINGS_SQL)["rows"]
            stage("bfs depth 6", f"SELECT count(*) FROM warren.bfs('{GRAPH}', ARRAY[1::bigint], 6)", 3, True)
            stage("bfs depth 6 count", f"SELECT count(*) FROM warren.bfs('{GRAPH}', ARRAY[1::bigint], 6)")
            stage("neighbours", f"SELECT count(*) FROM warren.neighbours('{GRAPH}', 1::bigint)", 3 if reloads else 20, True)
            stage("reachable_set", f"SELECT count(*) FROM warren.reachable_set('{GRAPH}', ARRAY[1::bigint])")
            self.stage(result, f"{label}: reachable_set stats", {"stats": s.run("SELECT warren.last_stats()")["rows"]})
            stage("components", f"SELECT count(*) FROM (SELECT component_id FROM warren.components('{GRAPH}', "
                                "NULL::bigint) GROUP BY 1) c")
            stage("pagerank", pagerank_sql(GRAPH))
            # From a cold cache: one statement, so it loads the version once
            # even when the version and its scratch exceed the cap (§5.5).
            s.run(f"SELECT warren.evict('{GRAPH}')")
            stage("pagerank from cold", pagerank_sql(GRAPH))
            if label == "default" and self.options.eigen:
                limit = f", max_iter := {self.options.eigen_max_iter}" if self.options.eigen_max_iter else ""
                stage("eigen", f"SELECT count(*), max(residual) FROM warren.eigen('{GRAPH}', 20, "
                               f"'normalized_laplacian', 'smallest', tol := {EIGEN_TOL}{limit})")

    def _eigen_steps(self, case, result):
        """warren-core's Lanczos on the same edges (as an undirected graph),
        streamed through a FIFO, for the step count SQL does not report."""
        import eigen

        fifo = Path(self.cluster.directory) / "edges.fifo"
        fifo.unlink(missing_ok=True)
        os.mkfifo(fifo, 0o666)
        os.chmod(fifo, 0o666)
        found = {}

        def solve():
            try:
                found.update(eigen.warren(fifo, case.nodes, "normalized_laplacian", "smallest", 20, 1e-6,
                                          self.options.threads, "f32"))
            except subprocess.CalledProcessError as error:
                found["error"] = error.stderr[-2000:]

        solver = threading.Thread(target=solve)
        solver.start()
        with self.session() as s:
            s.run(f"COPY (SELECT src, dst FROM soak_edges) TO '{fifo}' WITH (FORMAT csv, HEADER)")
        solver.join()
        found.pop("eigenvalues", None)
        found["peak_rss"] = resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss * 1024
        result["stages"]["eigen_core"] = found
        fifo.unlink()

    def _second_version(self, case, result, v1):
        free = free_disk(self.options.parent)
        if not disk_allows(free, v1["bytes"] + GIB, self.options.disk_floor):
            result["skipped"]["second_version"] = (f"disk: {size(free)} free, a second version of "
                                                   f"{size(v1['bytes'])} would leave less than the floor")
            return
        with self.session() as s:
            for sql in remove_sql(case):
                s.run(sql)
        with self.session() as s:
            self.stage(result, "refresh 2", s.measure(f"SELECT warren.refresh('{GRAPH}')"))
        v2 = self._version_stats(result, "version_2")
        with self.session() as s:
            s.run(f"SET warren.threads = {self.options.threads}")
            pair = f"'{GRAPH}', {v1['version']}, {v2['version']}"
            self.stage(result, "diff", s.measure(f"SELECT warren.diff({pair})"))
            for method in ("edit", "degree"):
                self.stage(result, f"distance {method}", s.measure(f"SELECT warren.distance({pair}, '{method}')"))


PICK = {  # verdict key: (stage, field)
    "bfs_seconds": ("default: bfs depth 6", "server_median_seconds"),
    "bfs_cte_seconds": ("bfs_cte", "server_median_seconds"),
    "components_seconds": ("default: components", "median_seconds"),
    "pagerank_seconds": ("default: pagerank", "median_seconds"),
    "eigen_seconds": ("default: eigen", "median_seconds"),
    "refresh_seconds": ("refresh", "median_seconds"),
    "refresh_peak_bytes": ("refresh", "vm_hwm"),
    "diff_seconds": ("diff", "median_seconds"),
    "cold_load_seconds": ("default: prewarm", "median_seconds"),
}


SKIPPED_BY = {"bfs_cte": "bfs_cte", "diff": "second_version"}  # stage: its key in `skipped`


def unreached(result, name):
    """Why planned stage `name` has no measurement, or None if it was not
    planned or was skipped (for disk, or by option)."""
    skipped = result.get("skipped", {})
    if name not in result.get("planned_stages", ()) or {"case", SKIPPED_BY.get(name)} & skipped.keys():
        return None
    return f"not reached: {result['aborted']}" if result.get("aborted") else "not reached"


def outcome(result, name):
    """Stage `name` if it finished, else (None, why): its first error, or
    why it never ran (None if that is no miss)."""
    stage = result["stages"].get(name)
    if stage is None:
        return None, unreached(result, name)
    errors = stage.get("errors")
    return (stage, None) if not errors else (None, errors[0])


def summary(result):
    """The numbers the verdicts read, from a case's stages: `<key>_failed`
    holds the error of a stage that failed or was never reached, and
    `<key>_stopped` marks a cancelled one, its time a lower bound."""
    stages, measured = result["stages"], {"cores": result["hardware"]["cpus"]}
    for key, (name, field) in PICK.items():
        stage, why = outcome(result, name)
        errors = stages.get(name, {}).get("errors", [])
        if stage is not None:
            measured[key] = stage[field]
        elif any(STOPPED in error for error in errors):
            if key.endswith("_seconds"):
                measured[key], measured[f"{key}_stopped"] = stages[name][field], True
        elif why is not None:
            measured[f"{key}_failed"] = why
    load, _ = outcome(result, "default: prewarm")
    if load is not None and "|" in load["rows"][0]:
        measured["load_segment_reads"] = int(load["rows"][0].split("|")[0])
    for name, prefix in (("default: pagerank", "pagerank"), ("default: pagerank from cold", "pagerank_cold")):
        pagerank, _ = outcome(result, name)
        if pagerank is None or "|" not in pagerank["rows"][0]:
            continue
        reads = int(pagerank["rows"][0].split("|")[0])
        measured[f"{prefix}_segment_reads"] = reads
        if prefix == "pagerank_cold":
            measured["pagerank_cold_seconds"] = pagerank["median_seconds"]
        if measured.get("load_segment_reads"):
            measured[f"{prefix}_loads"] = reads / measured["load_segment_reads"]
    eigen, _ = outcome(result, "default: eigen")
    if eigen is not None:
        measured["eigen_residual"] = float(eigen["rows"][0].split("|")[1])
        measured["eigen_converged"] = not any(UNCONVERGED in warning for warning in eigen.get("warnings", []))
        if result.get("eigen_max_iter"):
            measured["eigen_seconds_per_step"] = measured.pop("eigen_seconds") / result["eigen_max_iter"]
    reachable, why = outcome(result, "default: reachable_set")
    stats = stages.get("default: reachable_set stats", {}).get("stats")
    if reachable and stats:
        # A pulled level scans fewer row entries than the component has
        # edges (DESIGN.md §9.3): the rate is the version's edges (the
        # start's component is all but a few of its nodes) per second.
        seconds = reachable["median_seconds"]
        measured["reachable_row_entries_per_second"] = json.loads(stats[0])["edges_traversed"] / seconds
        measured["reachable_edges_per_second"] = result["version_1"]["edge_count"] / seconds
    elif why is not None:
        measured["reachable_edges_per_second_failed"] = why
    _, why = outcome(result, "cold_load_probe")
    if result.get("cached_bytes"):
        measured.update(cached_bytes=result["cached_bytes"], id_space=result["version_1"]["id_space"],
                        max_cache_memory=result.get("cache_memory_bytes", DEFAULT_CAP))
    elif why is not None:
        measured["cached_bytes_failed"] = why
    return measured


def git_revision():
    """The harness checkout's commit, and whether `bench/` or `test/` differ from it."""
    def git(*arguments):
        done = subprocess.run(["git", *arguments], cwd=HERE, capture_output=True, text=True)
        return done.stdout.strip() if done.returncode == 0 else None

    revision = git("rev-parse", "HEAD")
    status = git("status", "--porcelain", "--", str(HERE), str(HERE.parent / "test"))
    return revision, None if status is None else bool(status)


def provenance(argv, revision, profile, library):
    """What produced a result: the command, the harness's revision, and the
    extension build (its revision and profile as given, and the library
    the cluster loads)."""
    harness, dirty = git_revision()
    return {"argv": argv, "harness": {"revision": harness, "dirty": dirty},
            "extension": {"revision": revision, "profile": profile, "library": str(library),
                          "library_sha256": hashlib.sha256(Path(library).read_bytes()).hexdigest()}}


def installed_library(pg_config):
    done = subprocess.run([pg_config, "--pkglibdir"], capture_output=True, text=True, check=True)
    return Path(done.stdout.strip()) / "pg_warren.so"


def _now():
    return datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds")


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("cases", nargs="*", help=f"cases to run (default: all of {', '.join(CASES)})")
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"),
                         help="the Postgres tree with a release build of pg_warren installed")
    options.add_argument("--extension-revision", required=True,
                         help="the git revision the installed pg_warren was built from (recorded with the results)")
    options.add_argument("--extension-profile", required=True, choices=["release", "debug"],
                         help="the cargo profile of the installed build (recorded with the results)")
    options.add_argument("--runas", default="runner", help="cluster user when run as root")
    options.add_argument("--parent", type=Path, default=Path("/var/tmp"), help="where the cluster directory goes")
    options.add_argument("--results", type=Path, default=HERE / "results" / "soak")
    options.add_argument("--scale", type=int, default=1, help="divide nodes and edges by this (quick checks)")
    options.add_argument("--threads", type=int, default=os.cpu_count())
    options.add_argument("--cache-memory-kb", type=int,
                         help="warren.max_cache_memory of the first pass, in kB (default: the 4GB default)")
    options.add_argument("--raised-cache-memory", default="8GB",
                         help="warren.max_cache_memory for the second pass of a case that does not fit")
    options.add_argument("--statement-timeout", help="statement_timeout of the read workloads, e.g. 15min")
    options.add_argument("--skip", action="append", default=[], choices=["second-version"],
                         help="leave out a part of each case")
    options.add_argument("--label", default="", help="suffix for the result file name")
    options.add_argument("--disk-floor", type=int, default=4 * GIB, help="bytes of disk to keep free")
    options.add_argument("--no-cte", dest="cte", action="store_false", help="skip the recursive CTE baseline")
    options.add_argument("--no-eigen", dest="eigen", action="store_false", help="skip eigen (and its steps)")
    options.add_argument("--eigen-max-iter", type=int, help="cap eigen's operator applications (per-step cost)")
    options.add_argument("--no-eigen-steps", dest="eigen_steps", action="store_false",
                         help="skip warren-core's Lanczos on the same edges")
    args = options.parse_args(argv)
    args.cases = args.cases or list(CASES)
    if unknown := set(args.cases) - set(CASES):
        options.error(f"unknown cases: {', '.join(sorted(unknown))}")
    args.results.mkdir(parents=True, exist_ok=True)
    found = provenance([sys.argv[0], *argv], args.extension_revision, args.extension_profile,
                       installed_library(args.pg_config))
    with Cluster(args.pg_config, args.runas, parent=args.parent, settings=SETTINGS) as cluster:
        soak = Soak(cluster, args, found)
        for name in args.cases:
            case = CASES[name].scaled(args.scale) if args.scale > 1 else CASES[name]
            print(f"{name}: {case.nodes} nodes, {case.edges} edges, free disk {size(free_disk(args.parent))}",
                  flush=True)
            result = soak.run(case)
            suffix = f"-scale{args.scale}" if args.scale > 1 else ""
            out = args.results / f"{name}{suffix}{args.label}.json"
            out.write_text(json.dumps(result, indent=1) + "\n")
            for verdict in result["verdicts"]:
                print(f"  {'met ' if verdict['met'] else 'MISS'} {verdict['workload']}: {verdict['measured']} "
                      f"(target {verdict['target']})", flush=True)
            print(f"  wrote {out}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
