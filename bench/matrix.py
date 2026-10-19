#!/usr/bin/env python3
"""Runs the benchmark matrix (DESIGN.md §12 "Performance"): for each graph in
matrix.json, generate it, load it into `nodes`/`edges` of a scratch cluster,
run the shared setup, then time every workload. A workload whose `requires`
functions do not exist yet is recorded as skipped, so later functions slot
in by adding a matrix entry. Results go to
`<results>/<pg_warren version>/<graph>.json`.

Matrix format: `graphs` maps a name to `generate` (bench/generate.py
arguments); `setup` is SQL run once per graph; `params` is a one-row query
whose columns replace `{name}` in workload SQL; each workload has `name`,
`sql`, optional `requires` (function names) and `repeat` (default 3). A
workload that errors is recorded with its error, and the run exits 1."""

import argparse
import datetime
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "test"))

from harness import extension  # noqa: E402
from harness.cluster import Cluster  # noqa: E402
from harness.runner import Fixtures  # noqa: E402

TIMER = """CREATE FUNCTION bench_seconds(statement text) RETURNS float8 LANGUAGE plpgsql AS $$
DECLARE started timestamptz := clock_timestamp();
BEGIN EXECUTE statement; RETURN extract(epoch FROM clock_timestamp() - started); END $$"""


def plan(workloads, available):
    """Each workload with the reason it is skipped, or None if it runs."""
    planned = []
    for workload in workloads:
        missing = [f for f in workload.get("requires", []) if f not in available]
        planned.append((workload, f"missing {', '.join(missing)}" if missing else None))
    return planned


def substitute(sql, params):
    for name, value in params.items():
        sql = sql.replace("{" + name + "}", value)
    return sql


def available_functions(database):
    return {row[0] for row in database.query(
        "SELECT DISTINCT n.nspname || '.' || p.proname FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace "
        "WHERE n.nspname = 'warren'")}


def run_graph(cluster, name, graph, matrix, fixtures):
    database_name = f"bench_{name.replace('-', '_')}"
    cluster.database().execute(f'DROP DATABASE IF EXISTS "{database_name}"')
    cluster.database().execute(f'CREATE DATABASE "{database_name}"')
    database = cluster.database(database_name)
    database.execute("CREATE EXTENSION pg_warren")
    database.execute(TIMER)
    fixture = fixtures.path(graph["generate"])
    database.run_file_checked(fixture / "load.sql", cwd=fixture)
    for statement in matrix["setup"]:
        database.execute(statement)
    params = dict(database.query(
        f"SELECT key, value FROM jsonb_each_text((SELECT to_jsonb(p) FROM ({matrix['params']}) p LIMIT 1))"))
    results = []
    for workload, skipped in plan(matrix["workloads"], available_functions(database)):
        entry = {"name": workload["name"], "sql": workload["sql"]}
        if skipped:
            entry["skipped"] = skipped
        else:
            sql = substitute(workload["sql"], params)
            try:
                seconds = [float(database.query(f"SELECT bench_seconds($bench${sql}$bench$)")[0][0])
                           for _ in range(workload.get("repeat", 3))]
                entry.update(seconds=seconds, median=statistics.median(seconds))
            except RuntimeError as error:
                entry["error"] = str(error)
        print(f"  {workload['name']}: " + (skipped or entry.get("error") or f"{entry['median']:.3f}s"), flush=True)
        results.append(entry)
    truth = json.loads((fixture / "truth.json").read_text())
    return {
        "graph": name, "generate": graph["generate"], "nodes": truth["nodes"], "edges": truth["edges"],
        "params": params, "pg_warren": database.query("SELECT warren.version()")[0][0],
        "postgres": database.query("SELECT current_setting('server_version')")[0][0], "git": _git_revision(),
        "cpus": os.cpu_count(), "machine": platform.machine(),
        "recorded_at": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="seconds"),
        "workloads": results,
    }


def _git_revision():
    done = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=HERE, capture_output=True, text=True)
    return done.stdout.strip() or None


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("graphs", nargs="*", help="graph names from the matrix (default: all but smoke)")
    options.add_argument("--matrix", type=Path, default=HERE / "matrix.json")
    options.add_argument("--results", type=Path, default=HERE / "results")
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--no-install", action="store_true", help="use the pg_warren already installed")
    options.add_argument("--runas", default="postgres", help="cluster user when run as root")
    args = options.parse_args(argv)
    matrix = json.loads(args.matrix.read_text())
    names = args.graphs or [g for g in matrix["graphs"] if g != "smoke"]
    failed = []

    with extension.shared_lock():
        if not args.no_install:
            extension.install(args.pg_config, release=True)
        with tempfile.TemporaryDirectory(prefix="warren-bench-") as scratch, Cluster(args.pg_config, args.runas) as cluster:
            for name in names:
                print(f"{name}:", flush=True)
                result = run_graph(cluster, name, matrix["graphs"][name], matrix, Fixtures(scratch))
                out = args.results / result["pg_warren"] / f"{name}.json"
                out.parent.mkdir(parents=True, exist_ok=True)
                out.write_text(json.dumps(result, indent=1) + "\n")
                print(f"  wrote {out}")
                failed += [f"{name}/{w['name']}" for w in result["workloads"] if "error" in w]
    if failed:
        print(f"workloads failed: {', '.join(failed)}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
