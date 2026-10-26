"""What the pooler soak samples, at intervals, while its clients run
(DESIGN.md §13 M11): each pooled server backend's memory as the kernel
sees it (`/proc/<pid>/smaps_rollup`) and as Postgres does (the memory
context totals `pg_log_backend_memory_contexts` writes to the server
log), pg_warren's own accounting (`warren.backends()`'s private,
transient and mapped bytes per backend; `warren.shared_cache()`), the
activity and progress rows, the DSM segments the cluster's processes map,
the catalog's rows and table sizes, and each pooler's `SHOW POOLS` and
`SHOW STATS`.

A sample is a list of rows `(elapsed seconds, metric, subject, value)`;
the subject is `cluster`, a pool mode, or `<mode>/<pid>` for a server
backend."""

import re
import shutil
from pathlib import Path

ROLLUP_FIELDS = {"Rss": "rss", "Pss": "pss", "Pss_Anon": "pss_anon", "Pss_Shmem": "pss_shmem",
                 "Anonymous": "anonymous"}
DSM = re.compile(r"^([0-9a-f]+)-([0-9a-f]+) \S+ \S+ \S+ \S+\s+/dev/shm/(PostgreSQL\.\d+)\s*$", re.M)
GRAND_TOTAL = re.compile(r"\[(\d+)\] \S+ LOG:\s+Grand total: (\d+) bytes")

BACKENDS = ("SELECT a.usename, a.pid, coalesce(b.private_bytes, 0), coalesce(b.transient_bytes, 0), "
            "coalesce(b.mapped_bytes, 0) FROM pg_stat_activity a "
            "LEFT JOIN warren.backends() b ON b.pid = a.pid AND b.kind = 'backend' "
            "WHERE a.backend_type = 'client backend' AND a.usename = ANY ('{{{roles}}}')")
CLUSTER_METRICS = {
    "activity_rows": "SELECT count(*) FROM warren.backends() WHERE kind = 'backend'",
    "progress_rows": "SELECT count(*) FROM warren.progress()",
    "shared_bytes": "SELECT coalesce(sum(bytes), 0) FROM warren.shared_cache() WHERE kind = 'cluster'",
    "shared_entries": "SELECT count(*) FROM warren.shared_cache() WHERE kind IN ('version', 'vector')",
    "shared_pinned": "SELECT count(*) FROM warren.shared_cache() WHERE kind IN ('version', 'vector') AND pins > 0",
    "promoted_versions": "SELECT count(*) FROM warren.versions WHERE NOT staged",
    "staged_versions": "SELECT count(*) FROM warren.versions WHERE staged",
    "segments": "SELECT count(*) FROM warren.segments",
    "vectors": "SELECT count(*) FROM warren.vectors",
    "vector_chunks": "SELECT count(*) FROM warren.vector_chunks",
    **{f"{table}_bytes": f"SELECT pg_total_relation_size('warren.{table}')"
       for table in ("graphs", "versions", "segments", "vectors", "vector_chunks")},
}
POOL_FIELDS = ("cl_active", "cl_waiting", "sv_active", "sv_idle", "sv_used", "maxwait")
STATS_FIELDS = ("total_xact_count", "total_query_count", "avg_xact_time", "avg_wait_time")


def smaps_rollup(text):
    """A process's memory, in bytes, from `/proc/<pid>/smaps_rollup`."""
    found = {}
    for line in text.splitlines():
        key, _, value = line.partition(":")
        if key in ROLLUP_FIELDS:
            found[ROLLUP_FIELDS[key]] = int(value.split()[0]) * 1024
    return found


def dsm_segments(maps):
    """The POSIX DSM segments a process maps (`/proc/<pid>/maps`), by
    name, with the bytes it maps of each."""
    found = {}
    for start, end, name in DSM.findall(maps):
        found[name] = found.get(name, 0) + int(end, 16) - int(start, 16)
    return found


def context_totals(log):
    """Each backend's last memory context total (`Grand total`, bytes) in server log text `log`."""
    return {int(pid): int(total) for pid, total in GRAND_TOTAL.findall(log)}


def children(parent):
    """The pids of `parent`'s child processes."""
    found = []
    for stat in Path("/proc").glob("[0-9]*/stat"):
        try:
            fields = stat.read_text().rsplit(")", 1)[1].split()
        except (FileNotFoundError, ProcessLookupError, IndexError):
            continue
        if int(fields[1]) == parent:
            found.append(int(stat.parent.name))
    return found


def read_proc(pid, name):
    try:
        return Path(f"/proc/{pid}/{name}").read_text()
    except (FileNotFoundError, ProcessLookupError, PermissionError):
        return None


class LogTail:
    """The lines a growing log file gained since the last `read`."""

    def __init__(self, path):
        self.path, self.offset = Path(path), 0

    def read(self):
        with open(self.path, "rb") as log:
            log.seek(self.offset)
            data = log.read()
        complete = data[:data.rfind(b"\n") + 1]
        self.offset += len(complete)
        return complete.decode(errors="replace")


class Sampler:
    """Samples `cluster` through the superuser's direct session `admin`
    (harness.session); `pools` maps each pool mode to its pooler and role.
    Every `context_every`-th sample asks each pooled backend to log its
    memory contexts; the totals are read from the log at the next one."""

    def __init__(self, cluster, admin, pools, context_every):
        self.cluster, self.admin, self.pools = cluster, admin, pools
        self.context_every, self.count = context_every, 0
        self.log = LogTail(cluster.log)
        self.postmaster = int((Path(cluster.directory) / "data" / "postmaster.pid").read_text().split()[0])

    def sample(self, elapsed):
        rows = []
        for metric, sql in CLUSTER_METRICS.items():
            rows.append((elapsed, metric, "cluster", float(self.admin.run(sql).value())))
        rows += self._dsm(elapsed)
        rows += self._backends(elapsed)
        for mode, (pooler, _) in self.pools.items():
            rows += self._pooler(elapsed, mode, pooler)
        self.count += 1
        return rows

    def _backends(self, elapsed):
        modes = {role: mode for mode, (_, role) in self.pools.items()}
        done = self.admin.run(BACKENDS.format(roles=",".join(modes)))
        if not done.ok:
            raise RuntimeError(f"sampling backends: {done.message}")
        totals = context_totals(self.log.read())
        rows, pids = [], []
        for role, pid, private, transient, mapped in done.rows:
            subject = f"{modes[role]}/{pid}"
            pids.append(pid)
            rows += [(elapsed, "warren_private", subject, float(private)),
                     (elapsed, "warren_transient", subject, float(transient)),
                     (elapsed, "warren_mapped", subject, float(mapped))]
            rollup = read_proc(pid, "smaps_rollup")
            if rollup is not None:
                rows += [(elapsed, metric, subject, float(value)) for metric, value in smaps_rollup(rollup).items()]
            if int(pid) in totals:
                rows.append((elapsed, "contexts", subject, float(totals[int(pid)])))
        if pids and self.count % self.context_every == 0:
            self.admin.run("SELECT count(pg_log_backend_memory_contexts(pid)) "
                           f"FROM unnest('{{{','.join(pids)}}}'::int[]) pid")
        return rows

    def mapped(self):
        """The DSM segments the cluster's processes map, by name, with their bytes."""
        segments = {}
        for pid in (self.postmaster, *children(self.postmaster)):
            maps = read_proc(pid, "maps")
            for name, size in (dsm_segments(maps) if maps else {}).items():
                segments[name] = max(segments.get(name, 0), size)
        return segments

    def dsm_count(self):
        return len(self.mapped())

    def _dsm(self, elapsed):
        segments = self.mapped()
        return [(elapsed, "dsm_segments", "cluster", float(len(segments))),
                (elapsed, "dsm_bytes", "cluster", float(sum(segments.values()))),
                (elapsed, "dev_shm_used", "machine", float(shutil.disk_usage("/dev/shm").used))]

    @staticmethod
    def _pooler(elapsed, mode, pooler):
        rows = []
        for row in pooler.show("POOLS"):
            if row.get("database") == pooler.database:
                rows += [(elapsed, f"pool_{field}", mode, float(row[field])) for field in POOL_FIELDS if field in row]
        for row in pooler.show("STATS"):
            if row.get("database") == pooler.database:
                rows += [(elapsed, f"pool_{field}", mode, float(row[field])) for field in STATS_FIELDS if field in row]
        return rows
