#!/usr/bin/env python3
"""The parallel measurement of the shared cache (DESIGN.md §5.6, measurement
plan; §14 item 33): per-row graph reads in a scan that Postgres may run in
parallel, before and after the graph reads become `PARALLEL SAFE`.

For each mode (`preload`: `shared_preload_libraries = 'pg_warren'`, so the
shared cache exists on every major; `private`: no preload, which on PG 15
and 16 leaves the shared cache unavailable, so every backend, a parallel
worker included, loads its own copy), each label (`restricted`, the
functions' label before §14 item 33; `safe`, after, set with `ALTER
FUNCTION` in the benchmark's database), each probe table size and each
`max_parallel_workers_per_gather`, every workload runs `--repeat` times in
one session whose leader has already read the graph (as an application's
connection would have), under `EXPLAIN (ANALYZE, TIMING OFF)`: the
execution time and the workers launched. With workers, the planner's
parallel costs are set to 0 so that the plan is parallel wherever the label
allows it; `planned` repeats the run at the default costs, to show what the
planner picks by itself.

    bench/parallel.py --pg-config /path/to/pg_config \\
        --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release

Writes `<results>/<case>[-scaleS]<label>.json`."""

import argparse
import json
import os
import re
import statistics
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "test"))

import soak  # noqa: E402
from harness.cluster import Cluster  # noqa: E402

DATABASE = "parallel"
SEED = soak.SEED
SETTINGS = ("shared_buffers = '1GB'", "autovacuum = off", "max_wal_size = '1GB'",
            "max_worker_processes = 16", "max_parallel_workers = 8")
MODES = ("preload", "private")
LABELS = ("restricted", "safe")
# The functions §14 item 33 makes `PARALLEL SAFE`, by their SQL signatures.
FLIPPED = (
    "warren.neighbours(text, anyelement, text, text[], bigint)",
    "warren.degree(text, anyelement, text, text[], bigint)",
    "warren.has_edge(text, anyelement, anyelement, text, bigint)",
)
WORKLOADS = {
    "has_edge": "SELECT count(*) FROM probe WHERE warren.has_edge('g1', src, dst)",
    "degree": "SELECT sum(warren.degree('g1', src)) FROM probe",
    "neighbours": "SELECT count(*) FROM probe, LATERAL warren.neighbours('g1', src) n",
}
FORCED = ("SET parallel_setup_cost = 0", "SET parallel_tuple_cost = 0",
          "SET min_parallel_table_scan_size = 0")


def probe_sql(rows, nodes, seed):
    """A table of `rows` pairs of nodes: half random pairs, half an edge's
    endpoints, so `has_edge` answers both ways."""
    return ("DROP TABLE IF EXISTS probe; "
            f"CREATE TABLE probe AS SELECT src, CASE WHEN i % 2 = 0 THEN dst "
            f"ELSE 1 + floor(random() * {nodes})::bigint END AS dst "
            f"FROM (SELECT setseed({seed}) OFFSET 0) seeded, "
            f"LATERAL (SELECT row_number() OVER () AS i, src::bigint, dst::bigint FROM soak_edges "
            f"LIMIT {rows}) e; ANALYZE probe")


def label_sql(label):
    """The statement that gives the flipped functions `label`."""
    word = "SAFE" if label == "safe" else "RESTRICTED"
    return "; ".join(f"ALTER FUNCTION {function} PARALLEL {word}" for function in FLIPPED)


def session_commands(workload, workers, forced, repeat):
    """The statements of one measured session: settings, a read of the
    graph in the leader, then the workload `repeat` times under EXPLAIN."""
    commands = [f"SET max_parallel_workers_per_gather = {workers}"]
    if forced and workers > 0:
        commands += FORCED
    commands.append("SELECT warren.degree('g1', 1::bigint)")
    explain = f"EXPLAIN (ANALYZE, TIMING OFF, COSTS OFF) {WORKLOADS[workload]}"
    return commands + [explain] * repeat


EXECUTION = re.compile(r"Execution Time: ([\d.]+) ms")
LAUNCHED = re.compile(r"Workers Launched: (\d+)")


def explained(output, repeat):
    """Each run's milliseconds and the workers it launched, from the
    session's EXPLAIN ANALYZE output; the plan of the last run."""
    plans = output.split("Execution Time:")
    times = [float(t) for t in EXECUTION.findall(output)]
    launched = []
    for plan in plans[:len(times)]:
        found = LAUNCHED.findall(plan)
        launched.append(sum(int(n) for n in found))
    last = plans[len(times) - 1] if times else output
    return {"ms": times[:repeat], "workers_launched": launched[:repeat],
            "plan": [line.strip() for line in last.splitlines()
                     if line.strip() and not line.strip().startswith(("Planning", "Execution", "("))
                     and not re.match(r"^-?\d", line.strip())][-12:]}


def summary(found):
    """The median and spread of a run's times."""
    if not found["ms"]:
        return found
    return {**found, "median_ms": round(statistics.median(found["ms"]), 1),
            "min_ms": min(found["ms"]), "max_ms": max(found["ms"])}


class Parallel:
    def __init__(self, cluster, options):
        self.cluster, self.options = cluster, options
        self.admin = cluster.database(DATABASE)

    def measure(self, workload, workers, forced):
        commands = session_commands(workload, workers, forced, self.options.repeat)
        arguments = [argument for command in commands for argument in ("-c", command)]
        done = subprocess.run(self.admin.command("-At", *arguments), capture_output=True, text=True)
        if done.returncode != 0:
            return {"error": done.stderr.strip()[-1000:]}
        return summary(explained(done.stdout, self.options.repeat))

    def run(self, mode, label, rows):
        found = []
        for workload in self.options.workloads:
            for workers in self.options.workers:
                result = self.measure(workload, workers, forced=True)
                found.append({"mode": mode, "label": label, "rows": rows, "workload": workload,
                              "workers": workers, "costs": "forced" if workers else "default", **result})
                print(f"  {mode} {label} rows={rows} {workload} w={workers}: "
                      f"{result.get('median_ms', result.get('error'))} ms, "
                      f"launched {result.get('workers_launched')}", flush=True)
            result = self.measure(workload, max(self.options.workers), forced=False)
            found.append({"mode": mode, "label": label, "rows": rows, "workload": workload,
                          "workers": max(self.options.workers), "costs": "default", **result})
            print(f"  {mode} {label} rows={rows} {workload} planned: "
                  f"{result.get('median_ms', result.get('error'))} ms, "
                  f"launched {result.get('workers_launched')}", flush=True)
        return found


def mode_sql(mode):
    """The statement that configures a mode (a restart follows)."""
    if mode == "preload":
        return "ALTER SYSTEM SET shared_preload_libraries = 'pg_warren'"
    return "ALTER SYSTEM RESET shared_preload_libraries"


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("--case", default="a", choices=list(soak.CASES))
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"),
                         help="the Postgres tree with a release build of pg_warren installed")
    options.add_argument("--extension-revision", required=True)
    options.add_argument("--extension-profile", required=True, choices=["release", "debug"])
    options.add_argument("--runas", default="postgres", help="cluster user when run as root")
    options.add_argument("--parent", type=Path, default=Path("/var/tmp"), help="where the cluster directory goes")
    options.add_argument("--results", type=Path, default=HERE / "results" / "parallel")
    options.add_argument("--scale", type=int, default=10, help="divide the case's nodes and edges by this")
    options.add_argument("--rows", type=int, nargs="+", default=[100_000, 1_000_000],
                         help="probe table sizes")
    options.add_argument("--workers", type=int, nargs="+", default=[0, 2, 4])
    options.add_argument("--workloads", nargs="+", choices=list(WORKLOADS), default=list(WORKLOADS))
    options.add_argument("--modes", nargs="+", choices=MODES, default=list(MODES))
    options.add_argument("--labels", nargs="+", choices=LABELS, default=list(LABELS))
    options.add_argument("--repeat", type=int, default=3)
    options.add_argument("--label", default="", help="suffix for the result file name")
    args = options.parse_args(argv)
    args.results.mkdir(parents=True, exist_ok=True)
    case = soak.CASES[args.case].scaled(args.scale) if args.scale > 1 else soak.CASES[args.case]
    found = soak.provenance([sys.argv[0], *argv], args.extension_revision, args.extension_profile,
                            soak.installed_library(args.pg_config))
    result = {"provenance": found, "case": args.case, "nodes": case.nodes, "edges": case.edges,
              "repeat": args.repeat, "hardware": soak.hardware(), "started_at": soak._now(), "runs": []}
    with Cluster(args.pg_config, args.runas, parent=args.parent, settings=SETTINGS) as cluster:
        admin = cluster.database()
        admin.execute(f"CREATE DATABASE {DATABASE}")
        bench = Parallel(cluster, args)
        bench.admin.execute("CREATE EXTENSION pg_warren")
        for statement in soak.table_sql(case, SEED):
            bench.admin.execute(statement)
        bench.admin.execute(soak.create_graph_sql(case, SEED, name="g1", table=True))
        bench.admin.execute("SELECT warren.refresh('g1')")
        result["version_bytes"] = int(bench.admin.query(
            "SELECT v.bytes FROM warren.versions v JOIN warren.graphs g USING (graph_id) "
            "WHERE g.name = 'g1' AND v.version_id = g.current_version")[0][0])
        result["pg_warren"] = bench.admin.query("SELECT warren.version()")[0][0]
        result["postgres"] = bench.admin.query("SELECT current_setting('server_version')")[0][0]
        print(f"{args.case}: {case.nodes} nodes, {case.edges} edges, {result['version_bytes']} bytes", flush=True)
        for mode in args.modes:
            admin.execute(mode_sql(mode))
            cluster.restart()
            for rows in args.rows:
                bench.admin.execute(probe_sql(rows, case.nodes, SEED))
                for label in args.labels:
                    bench.admin.execute(label_sql(label))
                    result["runs"] += bench.run(mode, label, rows)
        result["finished_at"], result["load_average_after"] = soak._now(), os.getloadavg()
    suffix = f"-scale{args.scale}" if args.scale > 1 else ""
    out = args.results / f"{args.case}{suffix}{args.label}.json"
    out.write_text(json.dumps(result, indent=1) + "\n")
    print(f"wrote {out}", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
