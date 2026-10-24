#!/usr/bin/env python3
"""The real-graph suite (DESIGN.md §12, §13 M11): datasets from keepset,
fetched and pinned, loaded into `nodes`/`edges`, checked against NetworkX
and timed on the §12 matrix. bench/corpus/README.md has the details.

    bench/corpus/suite.py fetch --tier tiny            # fetch, pin, convert to tables
    bench/corpus/suite.py check --tier tiny --pg-config ...   # correctness against NetworkX
    bench/corpus/suite.py time --tier medium --pg-config ... --extension-revision ... --extension-profile release
    bench/corpus/suite.py ids hetionet yago3-10        # text id lengths and front coding

The cache (`--cache`, default $WARREN_CORPUS_CACHE or /var/tmp/warren-corpus)
holds the pinned keepset CLI and each dataset's tables; raw downloads are
deleted once converted."""

import argparse
import csv
import json
import os
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent), str(HERE.parent.parent / "test")]

from corpus import checks, fetch, ids, keepset, manifest, oracle, performance, tables  # noqa: E402
from harness.cluster import Cluster  # noqa: E402
from soak import SETTINGS, hardware, installed_library, provenance  # noqa: E402

RESULTS = HERE.parent / "results" / "corpus"


def _fetched(args, dataset):
    read = manifest.read(args.manifest)
    cli = keepset.Keepset(args.cache, read.keepset["version"])
    return fetch.fetch(dataset, cli, args.cache, lambda name, pin: manifest.record_pin(args.manifest, name, pin),
                       available_mb=None if args.ignore_memory else _available_mb())


def _available_mb():
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) // 1024
    return None


def command_fetch(args, datasets):
    for dataset in datasets:
        started = time.monotonic()
        directory = _fetched(args, dataset)
        described = json.loads((directory / "dataset.json").read_text())
        print(f"{dataset.name}: {described['tables']} ({time.monotonic() - started:.1f} s) in {directory}", flush=True)
    return 0


def _database(cluster, dataset, directory):
    name = "corpus_" + dataset.name.replace("-", "_")
    cluster.database().execute(f'DROP DATABASE IF EXISTS "{name}"')
    cluster.database().execute(f'CREATE DATABASE "{name}"')
    database = cluster.database(name)
    database.execute("CREATE EXTENSION pg_warren")
    database.run_file_checked(directory / "load.sql", cwd=directory)
    return name, database


def _write(path, result):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(result, indent=1) + "\n")
    print(f"  wrote {path}", flush=True)


def _provenance(args):
    return provenance([sys.argv[0], *args.argv], args.extension_revision, args.extension_profile,
                      installed_library(args.pg_config))


def command_check(args, datasets):
    failed = []
    with Cluster(args.pg_config, args.runas, parent=args.parent) as cluster:
        for dataset in datasets:
            directory = _fetched(args, dataset)
            print(f"{dataset.name}:", flush=True)
            started = time.monotonic()
            name, database = _database(cluster, dataset, directory)
            reference = oracle.Reference.read(directory, dataset)
            version, findings = checks.run(checks.Warren(database, dataset), reference, dataset,
                                           tables.create_graph_sql(dataset, "g"))
            for finding in findings:
                print(f"  {finding['status']:>11} {finding['check']}", flush=True)
            failed += [f"{dataset.name}/{f['check']}" for f in findings if f["status"] != "match"]
            cluster.database().execute(f'DROP DATABASE "{name}"')
            _write(args.results / "correctness" / f"{dataset.name}.json", {
                "provenance": _provenance(args), "hardware": hardware(), "dataset": dataset.name,
                "tier": dataset.tier, "fetched": json.loads((directory / "dataset.json").read_text()),
                "version": version, "seconds": round(time.monotonic() - started, 1), "findings": findings})
    print(f"not matching: {', '.join(failed)}" if failed else "every check matched", flush=True)
    return 1 if failed else 0


def command_ids(args, datasets):
    for dataset in datasets:
        if dataset.ids != "text":
            print(f"{dataset.name}: integer ids, skipped", flush=True)
            continue
        directory = _fetched(args, dataset)
        with open(directory / "nodes.csv", newline="", encoding="utf-8") as f:
            node_ids = [row[0] for row in csv.reader(f)]
        report = ids.report(node_ids, len(node_ids))
        found = report["distribution"]
        print(f"{dataset.name}: {found['count']} ids, mean {found['mean']:.1f} B, p99 {found['percentiles']['99']} B, "
              f"front coding {report['front_coded']['ratio']:.2f}x", flush=True)
        _write(args.results / "ids" / f"{dataset.name}.json", {"dataset": dataset.name, **report})
    return 0


def command_time(args, datasets):
    with Cluster(args.pg_config, args.runas, parent=args.parent, settings=SETTINGS) as cluster:
        for dataset in datasets:
            directory = _fetched(args, dataset)
            print(f"{dataset.name}:", flush=True)
            name, database = _database(cluster, dataset, directory)
            result = performance.run(cluster, name, database, dataset, args.threads, tables.create_graph_sql(dataset, "g"))
            cluster.database().execute(f'DROP DATABASE "{name}"')
            _write(args.results / f"{dataset.name}.json", {
                "provenance": _provenance(args), "hardware": hardware(), "dataset": dataset.name,
                "tier": dataset.tier, "fetched": json.loads((directory / "dataset.json").read_text()), **result})
    return 0


COMMANDS = {"fetch": command_fetch, "check": command_check, "ids": command_ids, "time": command_time}


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("command", choices=sorted(COMMANDS))
    options.add_argument("datasets", nargs="*", help="dataset names (default: every dataset of --tier)")
    options.add_argument("--tier", action="append", choices=manifest.TIERS, default=[],
                         help="the tiers to run (repeatable; default: tiny and medium)")
    options.add_argument("--manifest", type=Path, default=manifest.MANIFEST)
    options.add_argument("--cache", type=Path,
                         default=Path(os.environ.get("WARREN_CORPUS_CACHE", "/var/tmp/warren-corpus")))
    options.add_argument("--results", type=Path, default=RESULTS)
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"),
                         help="the Postgres tree with pg_warren installed (check, time)")
    options.add_argument("--extension-revision", default="unknown",
                         help="the git revision the installed pg_warren was built from (recorded)")
    options.add_argument("--extension-profile", default="unknown", choices=["release", "debug", "unknown"])
    options.add_argument("--runas", default="postgres", help="cluster user when run as root")
    options.add_argument("--parent", type=Path, default=Path("/var/tmp"), help="where the cluster directory goes")
    options.add_argument("--ignore-memory", action="store_true",
                         help="fetch even when keepset's heap estimate exceeds the available memory")
    options.add_argument("--threads", type=int, default=os.cpu_count(), help="warren.threads for timings")
    args = options.parse_args(argv)
    args.argv = argv
    read = manifest.read(args.manifest)
    datasets = read.select(names=args.datasets, tiers=args.tier or ["tiny", "medium"])
    return COMMANDS[args.command](args, datasets)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
