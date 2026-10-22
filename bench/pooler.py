#!/usr/bin/env python3
"""The pooler benchmark of the shared cache (DESIGN.md §5.6, measurement
plan; §12): many backends reading few graphs, with the shared cache off
(`warren.shared_cache_memory = 0`, each backend loads its own copy, as
0.4.0) and on (one copy per cluster, mapped by every backend). For each
case, graphs are generated inside a scratch cluster (as `bench/soak.py`
does), and for each mode and each number of server connections N:

- *cold first calls:* N sessions call `warren.neighbours` at once on a
  cold cache; each one's latency, timed in the server (one load per
  cluster when shared, one per backend when off);
- *pgbench* through PgBouncer in transaction mode, a pool of N server
  connections and N clients: `neighbours` of a random node, twice (the
  first run opens the pool's backends, so it includes their first calls),
  then a `bfs` to depth 3, each for `--seconds`: throughput and latency;
- *memory* with the pool's N backends alive and idle: each backend's
  `RssAnon`, `RssShmem` and PSS (`smaps_rollup`), the bytes of shared
  versions in `/dev/shm` and the registry's totals (`warren.shared_cache()`);
- one `pagerank` (20 iterations) through the pool.

Scenarios: `one-graph`, every client on one graph through the pool; and
`own-graph`, N graphs (the case scaled by `--own-scale`), each client on
its own graph over a direct connection, so nothing is shared and the
shared cache must cost nothing. A run whose private copies would not fit
`--memory-budget` (N backends each holding every graph it reads) is
skipped, and says so. Writes `<results>/<case>[-scaleS]<label>.json`.

    bench/pooler.py a b-unmerged --scale 10 --pg-config /path/to/pg_config \\
        --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
    bench/pooler.py a --scale 1000 --connections 1 4 --seconds 3 --extension-revision … \\
        --extension-profile debug   # a quick check

The cluster preloads pg_warren (`shared_preload_libraries`), so that the
shared cache exists on PG 15 and 16, and restarts between modes."""

import argparse
import json
import os
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "test"))

import soak  # noqa: E402
from harness.cluster import PORT, Cluster  # noqa: E402
from matrix import TIMER  # noqa: E402

GIB = 1 << 30
DATABASE = "pooler"
POOLER_PORT = 6432
SEED = soak.SEED
DEFAULT_CAP = "4GB"  # warren.shared_cache_memory's default (DESIGN.md §14 item 32)
SETTINGS = ("shared_buffers = '1GB'", "max_connections = 200", "autovacuum = off",
            "shared_preload_libraries = 'pg_warren'", "max_wal_size = '1GB'")
SCENARIOS = ("one-graph", "own-graph")
MEMORY_FIELDS = ("RssAnon", "RssShmem", "VmHWM")
SMAPS_FIELDS = ("Pss", "Pss_Anon", "Pss_Shmem")


def graph_names(scenario, connections):
    """The graphs `scenario` reads with `connections` server connections."""
    return ["g1"] if scenario == "one-graph" else [f"own{i + 1}" for i in range(connections)]


def script(workload, nodes, scenario):
    """A pgbench script: `workload` (`neighbours`, `bfs`) from a random node
    of a graph of `nodes`; on `own-graph`, each client reads its own graph."""
    graph = "'g1'" if scenario == "one-graph" else "'own' || (:client_id + 1)"
    call = {"neighbours": f"SELECT count(*) FROM warren.neighbours({graph}, :n::bigint)",
            "bfs": f"SELECT count(*) FROM warren.bfs({graph}, ARRAY[:n::bigint], 3)"}[workload]
    return f"\\set n random(1, {nodes})\n{call};\n"


def pgbouncer_config(directory, server_directory, server_port, pool_size, user):
    """PgBouncer in transaction mode, on a Unix socket in `directory`, in
    front of the cluster's socket, with a pool of `pool_size` server
    connections."""
    return (f"[databases]\n{DATABASE} = host={server_directory} port={server_port} dbname={DATABASE}\n"
            f"[pgbouncer]\nlisten_addr =\nlisten_port = {POOLER_PORT}\nunix_socket_dir = {directory}\n"
            f"auth_type = trust\nauth_file = {directory}/users.txt\npool_mode = transaction\n"
            f"default_pool_size = {pool_size}\nmin_pool_size = 0\nreserve_pool_size = 0\n"
            f"max_client_conn = {4 * pool_size + 16}\nserver_idle_timeout = 3600\n"
            f"logfile = {directory}/pgbouncer.log\npidfile = {directory}/pgbouncer.pid\n"
            f"admin_users = {user}\n")


PGBENCH = {
    "tps": re.compile(r"^tps = ([\d.]+) \(without initial connection time\)", re.M),
    "latency_ms": re.compile(r"^latency average = ([\d.]+) ms", re.M),
    "transactions": re.compile(r"^number of transactions actually processed: (\d+)", re.M),
    "failed": re.compile(r"^number of failed transactions: (\d+)", re.M),
}


def pgbench_summary(output):
    """Throughput, mean latency and transaction counts from pgbench's report."""
    found = {}
    for key, pattern in PGBENCH.items():
        match = pattern.search(output)
        if match:
            found[key] = float(match.group(1)) if "." in match.group(1) else int(match.group(1))
    return found


def memory_fields(status, smaps):
    """A backend's memory, in bytes, from `/proc/<pid>/status` and `smaps_rollup`."""
    found = {}
    for text, fields in ((status, MEMORY_FIELDS), (smaps, SMAPS_FIELDS)):
        for line in text.splitlines():
            key, _, value = line.partition(":")
            if key in fields:
                found[key] = int(value.split()[0]) * 1024
    return found


def totals(backends, shared_bytes):
    """The cluster's memory for the graphs: the backends' own (RssAnon, and
    PSS, which splits shared pages among their mappers) and the shared
    versions' segments, counted once."""
    anon = sum(b.get("RssAnon", 0) for b in backends)
    return {"backends": len(backends), "rss_anon": anon, "pss": sum(b.get("Pss", 0) for b in backends),
            "shared_versions": shared_bytes, "private_and_shared": anon + shared_bytes,
            "max_rss_anon": max((b.get("RssAnon", 0) for b in backends), default=0)}


def private_estimate(scenario, connections, graphs, version_bytes):
    """What the backends' own copies take when every version is private:
    each server connection of the pool may read every graph (`one-graph`
    has one); on `own-graph` each backend reads one."""
    per_backend = version_bytes if scenario == "own-graph" else graphs * version_bytes
    return connections * per_backend


def skip_reason(mode, scenario, connections, graphs, version_bytes, budget):
    """Why a run is skipped (private copies past the memory budget), or None."""
    if mode != "off":
        return None
    needed = private_estimate(scenario, connections, graphs, version_bytes)
    if needed > budget:
        return f"private copies need about {soak.size(needed)}, over the budget of {soak.size(budget)}"
    return None


def shared_cache_sql(cap):
    """The statements that set `warren.shared_cache_memory` for a mode (a
    restart follows)."""
    return f"ALTER SYSTEM SET warren.shared_cache_memory = '{cap}'"


def latencies(seconds):
    """A summary of per-session latencies."""
    if not seconds:
        return {}
    return {"seconds": [round(s, 4) for s in seconds], "median": round(statistics.median(seconds), 4),
            "max": round(max(seconds), 4), "min": round(min(seconds), 4)}


class Pooler:
    def __init__(self, cluster, options, provenance):
        self.cluster, self.options, self.provenance = cluster, options, provenance
        self.admin = cluster.database(DATABASE)

    # The cluster's state.

    def build(self, case, scenario, connections):
        """Creates and refreshes the graphs `scenario` needs that do not exist yet."""
        names = graph_names(scenario, connections)
        existing = {row[0] for row in self.admin.query("SELECT name FROM warren.graphs")}
        for name in names:
            if name in existing:
                continue
            graph_case = case if name == "g1" else case.scaled(self.options.own_scale)
            started = time.perf_counter()
            self.admin.execute(soak.create_graph_sql(graph_case, SEED, name=name, table=False))
            self.admin.execute(f"SELECT warren.refresh('{name}')")
            print(f"  built {name} ({graph_case.nodes} nodes) in {time.perf_counter() - started:.1f} s", flush=True)
        return names

    def version_bytes(self, name):
        rows = self.admin.query(f"SELECT v.bytes FROM warren.versions v JOIN warren.graphs g USING (graph_id) "
                                f"WHERE g.name = '{name}' AND v.version_id = g.current_version")
        return int(rows[0][0])

    def backend_pids(self):
        return [int(row[0]) for row in self.admin.query(
            f"SELECT pid FROM pg_stat_activity WHERE datname = '{DATABASE}' AND backend_type = 'client backend' "
            "AND pid <> pg_backend_pid()")]

    def reset(self, names):
        """A cold cache: every other backend ended, and the graphs' shared
        versions evicted (the caller owns them); waits until none is left."""
        self.admin.execute(f"SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = '{DATABASE}' "
                           "AND backend_type = 'client backend' AND pid <> pg_backend_pid()")
        for _ in range(100):
            if not self.backend_pids():
                break
            time.sleep(0.1)
        for name in names:
            self.admin.execute(f"SELECT warren.evict('{name}')")
        for _ in range(100):
            if self.shared_bytes() == 0:
                return
            time.sleep(0.1)

    def shared_bytes(self):
        """The bytes of every slot of the registry, from its totals row."""
        rows = self.admin.query("SELECT bytes FROM warren.shared_cache() WHERE kind = 'cluster'")
        return int(rows[0][0]) if rows and rows[0][0] else 0

    def memory(self):
        backends = []
        for pid in self.backend_pids():
            try:
                status = Path(f"/proc/{pid}/status").read_text()
                smaps = Path(f"/proc/{pid}/smaps_rollup").read_text()
            except FileNotFoundError:
                continue
            backends.append({"pid": pid, **memory_fields(status, smaps)})
        shm = shutil.disk_usage("/dev/shm")
        listing = self.admin.query("SELECT kind, state, bytes, pins, mappers FROM warren.shared_cache()")
        return {"backends": backends, "totals": totals(backends, self.shared_bytes()),
                "dev_shm_used": shm.used, "registry": listing}

    # Measurements.

    def cold_first_calls(self, names, connections):
        """N sessions' first calls at once, on a cold cache, timed in the server."""
        call = f"SELECT count(*) FROM warren.neighbours(''{{graph}}'', 1::bigint)"
        sessions = [names[i % len(names)] for i in range(connections)]
        results, errors = [None] * connections, []

        def run(i, graph):
            sql = f"SELECT bench_seconds('{call.format(graph=graph)}')"
            done = subprocess.run(self.admin.command("-At", "-c", sql), capture_output=True, text=True)
            if done.returncode != 0:
                errors.append(done.stderr.strip())
            else:
                results[i] = float(done.stdout.strip())

        threads = [threading.Thread(target=run, args=(i, g)) for i, g in enumerate(sessions)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        found = latencies([r for r in results if r is not None])
        if errors:
            found["errors"] = errors[:3]
        return found

    def pgbench(self, scenario, workload, nodes, connections, pooled):
        path = Path(self.cluster.directory) / f"{workload}-{scenario}.sql"
        path.write_text(script(workload, nodes, scenario))
        host = self.pooler_dir if pooled else str(self.cluster.directory)
        port = str(POOLER_PORT) if pooled else PORT
        command = [self.cluster.client("pgbench"), "-n", "-h", host, "-p", port, "-U", "postgres",
                   "-c", str(connections), "-j", str(min(connections, os.cpu_count())),
                   "-T", str(self.options.seconds), "-f", str(path), DATABASE]
        done = subprocess.run(command, capture_output=True, text=True)
        found = pgbench_summary(done.stdout)
        if done.returncode != 0:
            found["error"] = (done.stderr or done.stdout)[-1000:]
        print(f"    {workload} {'pooled' if pooled else 'direct'} c={connections}: "
              f"{found.get('tps', 0):.0f} tps, {found.get('latency_ms', 0):.3f} ms", flush=True)
        return found

    def pagerank(self, graph, pooled):
        host = self.pooler_dir if pooled else str(self.cluster.directory)
        port = str(POOLER_PORT) if pooled else PORT
        sql = f"SELECT bench_seconds($$SELECT warren.pagerank('{graph}', 'pooler_pr', tol := 0, max_iter := 20)$$)"
        command = [self.cluster.client("psql"), "-X", "-At", "-h", host, "-p", port, "-U", "postgres",
                   "-d", DATABASE, "-c", sql, "-c", f"SELECT warren.drop_vector('{graph}', 'pooler_pr', NULL, true)"]
        done = subprocess.run(command, capture_output=True, text=True)
        if done.returncode != 0:
            return {"error": done.stderr.strip()[-500:]}
        return {"seconds": float(done.stdout.split()[0])}

    # PgBouncer.

    def start_pooler(self, pool_size):
        directory = Path(self.cluster.directory) / "pgbouncer"
        directory.mkdir(exist_ok=True)
        (directory / "users.txt").write_text('"postgres" ""\n')
        config = directory / "pgbouncer.ini"
        config.write_text(pgbouncer_config(directory, self.cluster.directory, PORT, pool_size, "postgres"))
        if self.cluster.runas:
            shutil.chown(directory, self.cluster.runas)
            for path in directory.iterdir():
                shutil.chown(path, self.cluster.runas)
        command = [self.options.pgbouncer, "-d", str(config)]
        if self.cluster.runas:
            command = ["sudo", "-u", self.cluster.runas, "--", *command]
        subprocess.run(command, check=True, capture_output=True, text=True)
        self.pooler_dir = str(directory)
        for _ in range(50):
            if (directory / f".s.PGSQL.{POOLER_PORT}").exists():
                return
            time.sleep(0.1)
        raise RuntimeError(f"pgbouncer did not start: {(directory / 'pgbouncer.log').read_text()[-2000:]}")

    def stop_pooler(self):
        pid_file = Path(self.pooler_dir) / "pgbouncer.pid"
        if pid_file.exists():
            pid = pid_file.read_text().strip()
            subprocess.run(["sudo", "-u", self.cluster.runas, "--", "kill", pid] if self.cluster.runas
                           else ["kill", pid], capture_output=True)
            for _ in range(50):
                if not pid_file.exists():
                    break
                time.sleep(0.1)

    # A run.

    def run(self, case, mode, scenario, connections):
        names = self.build(case, scenario, connections)
        version_bytes = self.version_bytes(names[-1])
        result = {"mode": mode, "scenario": scenario, "connections": connections, "graphs": len(names),
                  "version_bytes": version_bytes}
        skipped = skip_reason(mode, scenario, connections, len(names), version_bytes, self.options.memory_budget)
        if skipped:
            print(f"  {mode} {scenario} N={connections}: skipped, {skipped}", flush=True)
            return {**result, "skipped": skipped}
        print(f"  {mode} {scenario} N={connections}", flush=True)
        nodes = case.nodes if scenario == "one-graph" else case.scaled(self.options.own_scale).nodes
        self.reset(names)
        result["memory_before"] = self.memory()["totals"]
        result["cold_first_calls"] = self.cold_first_calls(names, connections)
        self.reset(names)
        pooled = scenario == "one-graph"
        if pooled:
            self.start_pooler(connections)
        try:
            result["neighbours_first"] = self.pgbench(scenario, "neighbours", nodes, connections, pooled)
            result["neighbours"] = self.pgbench(scenario, "neighbours", nodes, connections, pooled)
            result["bfs"] = self.pgbench(scenario, "bfs", nodes, connections, pooled)
            if pooled:
                result["memory"] = self.memory()
                result["pagerank"] = self.pagerank(names[0], pooled)
                result["memory_after_pagerank"] = self.memory()["totals"]
            else:
                result["memory"] = self.memory_of_direct(scenario, nodes, connections)
        finally:
            if pooled:
                self.stop_pooler()
        return result

    def memory_of_direct(self, scenario, nodes, connections):
        """The memory of N direct sessions, each having read its own graph,
        measured while they are alive (pgbench's end their backends)."""
        holders = []
        for i in range(connections):
            command = self.admin.command("-At")
            process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            process.stdin.write(f"SELECT count(*) FROM warren.neighbours('own{i + 1}', 1::bigint);\n")
            process.stdin.flush()
            process.stdout.readline()
            holders.append(process)
        try:
            return self.memory()
        finally:
            for process in holders:
                process.stdin.close()
                process.wait()


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("cases", nargs="*", help=f"cases to run (default: a and b-unmerged; of {', '.join(soak.CASES)})")
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"),
                         help="the Postgres tree with a release build of pg_warren installed")
    options.add_argument("--extension-revision", required=True,
                         help="the git revision the installed pg_warren was built from (recorded with the results)")
    options.add_argument("--extension-profile", required=True, choices=["release", "debug"],
                         help="the cargo profile of the installed build (recorded with the results)")
    options.add_argument("--runas", default="postgres", help="cluster and pgbouncer user when run as root")
    options.add_argument("--parent", type=Path, default=Path("/var/tmp"), help="where the cluster directory goes")
    options.add_argument("--results", type=Path, default=HERE / "results" / "pooler")
    options.add_argument("--scale", type=int, default=10, help="divide the case's nodes and edges by this")
    options.add_argument("--own-scale", type=int, default=100,
                         help="divide the case further for each of own-graph's graphs")
    options.add_argument("--connections", type=int, nargs="+", default=[1, 10, 50])
    options.add_argument("--modes", nargs="+", choices=["off", "shared"], default=["off", "shared"])
    options.add_argument("--scenarios", nargs="+", choices=SCENARIOS, default=list(SCENARIOS))
    options.add_argument("--cap", default=DEFAULT_CAP, help="warren.shared_cache_memory when shared")
    options.add_argument("--seconds", type=int, default=20, help="each pgbench run's duration")
    options.add_argument("--memory-budget", type=int, default=int(0.6 * (soak.memory_total() or 8 * GIB)),
                         help="bytes the private copies of a run may take; runs that need more are skipped")
    options.add_argument("--pgbouncer", default=shutil.which("pgbouncer") or "/usr/sbin/pgbouncer")
    options.add_argument("--label", default="", help="suffix for the result file name")
    args = options.parse_args(argv)
    args.cases = args.cases or ["a", "b-unmerged"]
    if unknown := set(args.cases) - set(soak.CASES):
        options.error(f"unknown cases: {', '.join(sorted(unknown))}")
    args.results.mkdir(parents=True, exist_ok=True)
    found = soak.provenance([sys.argv[0], *argv], args.extension_revision, args.extension_profile,
                            soak.installed_library(args.pg_config))
    with Cluster(args.pg_config, args.runas, parent=args.parent, settings=SETTINGS) as cluster:
        for name in args.cases:
            case = soak.CASES[name].scaled(args.scale) if args.scale > 1 else soak.CASES[name]
            print(f"{name}: {case.nodes} nodes, {case.edges} edges", flush=True)
            result = {"provenance": found, "case": name, "nodes": case.nodes, "edges": case.edges,
                      "own_scale": args.own_scale, "seconds": args.seconds, "cap": args.cap,
                      "hardware": soak.hardware(), "started_at": soak._now(), "runs": []}
            admin = cluster.database()
            admin.execute(f"DROP DATABASE IF EXISTS {DATABASE}")
            admin.execute(f"CREATE DATABASE {DATABASE}")
            pooler = Pooler(cluster, args, found)
            pooler.admin.execute("CREATE EXTENSION pg_warren")
            pooler.admin.execute(TIMER)
            # The generator's marker of soak.py's second version, never set here.
            pooler.admin.execute("CREATE TABLE soak_removed ()")
            result["pg_warren"] = pooler.admin.query("SELECT warren.version()")[0][0]
            result["postgres"] = pooler.admin.query("SELECT current_setting('server_version')")[0][0]
            try:
                for mode in args.modes:
                    admin.execute(shared_cache_sql("0" if mode == "off" else args.cap))
                    cluster.restart()
                    for scenario in args.scenarios:
                        for connections in args.connections:
                            result["runs"].append(pooler.run(case, mode, scenario, connections))
            finally:
                admin.execute(f"DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)")
                result["finished_at"], result["load_average_after"] = soak._now(), os.getloadavg()
            suffix = f"-scale{args.scale}" if args.scale > 1 else ""
            out = args.results / f"{name}{suffix}{args.label}.json"
            out.write_text(json.dumps(result, indent=1) + "\n")
            print(f"  wrote {out}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
