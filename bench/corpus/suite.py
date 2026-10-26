#!/usr/bin/env python3
"""The real-graph suite (DESIGN.md §12, §13 M11): datasets from keepset,
fetched and pinned, loaded into `nodes`/`edges`, checked against NetworkX
and igraph and timed on the §12 matrix. bench/corpus/README.md has the details.

    bench/corpus/suite.py fetch --tier tiny            # fetch, pin, convert to tables
    bench/corpus/suite.py check --tier tiny --pg-config ...   # correctness against NetworkX and igraph
    bench/corpus/suite.py check --tier medium --oracle igraph --pg-config ...   # igraph alone
    bench/corpus/suite.py time --tier medium --pg-config ... --extension-revision ... --extension-profile release
    bench/corpus/suite.py ids hetionet yago3-10        # text id lengths and front coding
    bench/corpus/suite.py head-to-head facebook --pg-config ... --extension-revision ... --extension-profile release
    bench/corpus/suite.py summary                      # the results as Markdown tables

The cache (`--cache`, default $WARREN_CORPUS_CACHE or /var/tmp/warren-corpus)
holds the pinned keepset CLI and each dataset's tables; raw downloads are
deleted once converted."""

import argparse
import contextlib
import csv
import dataclasses
import json
import os
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path[:0] = [str(HERE.parent), str(HERE.parent.parent / "test")]

from corpus import checks, fetch, ids, keepset, manifest, oracle, performance, summary, tables  # noqa: E402
from corpus.head_to_head import answers, report, timing, workloads  # noqa: E402
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
            source = oracle.Source.read(directory, dataset)
            references = [oracle.LIBRARIES[name](source) for name in args.oracle]
            version, findings = checks.run(checks.Warren(database, dataset), source, references, dataset,
                                           tables.create_graph_sql(dataset, "g"))
            for finding in findings:
                print(f"  {finding['status']:>11} {finding['check']} ({finding['against']})", flush=True)
            failed += [f"{dataset.name}/{f['check']} ({f['against']})" for f in findings
                       if f["status"] not in checks.AGREEING]
            cluster.database().execute(f'DROP DATABASE "{name}"')
            _write(args.results / "correctness" / f"{dataset.name}.json", {
                "provenance": _provenance(args), "hardware": hardware(), "dataset": dataset.name,
                "tier": dataset.tier, "fetched": json.loads((directory / "dataset.json").read_text()),
                "oracles": {name: oracle.LIBRARIES[name].VERSION for name in args.oracle}, "version": version,
                "seconds": round(time.monotonic() - started, 1), "findings": findings})
    print(f"not matching: {', '.join(failed)}" if failed else "every check matched or differs as documented",
          flush=True)
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


def _engine(name, args, cluster, server, dataset, directory):
    from corpus.head_to_head import cypher_engine, gds_engine, igraph_engine, warren_engine
    if name == "pg_warren":
        return warren_engine.WarrenEngine(cluster, dataset, directory, tables.create_graph_sql(dataset, "g"),
                                          args.threads, args.timeout)
    if name == "igraph":
        return igraph_engine.IGraphEngine(dataset, directory)
    if name == "neo4j-cypher":
        return cypher_engine.CypherEngine(dataset, directory, server, args.timeout)
    return gds_engine.GdsEngine(dataset, directory, server, args.timeout, args.threads)


def _neo4j(args, engines):
    if not any(name.startswith("neo4j") for name in engines):
        return None
    from corpus.head_to_head import neo4j_server
    cache = args.cache / "head-to-head"
    server = neo4j_server.Server(neo4j_server.install(cache), cache / "neo4j-state", args.neo4j_heap,
                                 args.neo4j_pagecache)
    server.configure()
    return server


def command_head_to_head(args, datasets):
    engines = args.engine or list(report.ENGINES)
    server = _neo4j(args, engines)
    clustered = (Cluster(args.pg_config, args.runas, parent=args.parent, settings=SETTINGS) if "pg_warren" in engines
                 else contextlib.nullcontext())
    with clustered as cluster, contextlib.closing(server) if server else contextlib.nullcontext():
        for dataset in datasets:
            directory = _fetched(args, dataset)
            print(f"{dataset.name}:", flush=True)
            source = oracle.Source.read(directory, dataset)
            reference = answers.Reference(source, oracle.IGraphReference(source))
            planned = workloads.plan(source, args.betweenness_work)
            result = {"provenance": _provenance(args), "hardware": hardware(), "dataset": dataset.name,
                      "tier": dataset.tier, "fetched": json.loads((directory / "dataset.json").read_text()),
                      "repeat": args.repeat, "budget_seconds": args.budget, "timeout_seconds": args.timeout,
                      "workloads": [dataclasses.asdict(w) for w in planned], "engines": {}}
            for name in engines:
                engine = _engine(name, args, cluster, server, dataset, directory)
                try:
                    result["engines"][name] = timing.run(engine, planned, reference.check, args.repeat, args.budget)
                finally:
                    engine.close()
            if server is not None:
                result["neo4j_store_bytes"] = server.store_bytes()
                server.close()
            _write(args.results / "head_to_head" / f"{dataset.name}.json", result)
    return 0


def command_summary(args, datasets):
    def results(directory):
        found = [directory / f"{d.name}.json" for d in datasets]
        return [json.loads(path.read_text()) for path in found if path.exists()]

    print(summary.correctness(results(args.results / "correctness")))
    timed = results(args.results)
    if timed:
        print("\n" + summary.timings(timed, summary.SECTION_12), end="\n\n")
        print(summary.timings(timed, summary.LIBRARY))
    compared = results(args.results / "head_to_head")
    if compared:
        print("\n" + report.loads(compared), end="\n\n")
        print(report.workloads(compared), end="\n\n")
        print(report.unsupported(compared))
    return 0


COMMANDS = {"fetch": command_fetch, "check": command_check, "ids": command_ids, "time": command_time,
            "head-to-head": command_head_to_head, "summary": command_summary}


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
    options.add_argument("--oracle", action="append", choices=sorted(oracle.LIBRARIES), default=[],
                         help="the oracles `check` compares with (repeatable; default: every one)")
    options.add_argument("--threads", type=int, default=os.cpu_count(),
                         help="warren.threads for timings, and GDS's concurrency in the head-to-head")
    options.add_argument("--engine", action="append", choices=report.ENGINES, default=[],
                         help="the engines `head-to-head` runs (repeatable; default: every one)")
    options.add_argument("--repeat", type=int, default=5, help="head-to-head: warm runs of each workload")
    options.add_argument("--budget", type=float, default=120,
                         help="head-to-head: seconds after which no more warm runs start (one always runs)")
    options.add_argument("--timeout", type=int, default=600, help="head-to-head: seconds a run may take")
    options.add_argument("--betweenness-work", type=int, default=workloads.BETWEENNESS_WORK,
                         help="head-to-head: the largest N·M exact betweenness runs on")
    options.add_argument("--neo4j-heap", default="4g", help="head-to-head: Neo4j's JVM heap (initial and max)")
    options.add_argument("--neo4j-pagecache", default="2g", help="head-to-head: Neo4j's page cache")
    args = options.parse_args(argv)
    args.argv = argv
    args.oracle = args.oracle or sorted(oracle.LIBRARIES)
    read = manifest.read(args.manifest)
    datasets = read.select(names=args.datasets, tiers=args.tier or ["tiny", "medium"])
    return COMMANDS[args.command](args, datasets)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
