#!/usr/bin/env python3
"""Appendix A's use cases at 10M nodes (DESIGN.md Appendix A, §12, §13
M11): each use case of `use_cases/` generates its planted structure inside
a scratch cluster (no fixture files), runs its program in stages, each
timed with the backend's peak memory (`bench/soak.py`'s session: `/proc`
VmHWM and the RssAnon peak polled every 20 ms), and checks its expected
outcomes at that scale. Writes `<results>/<case>[-scaleN].json` with the
sizes, the stages, the checks, the verdicts against Appendix A's scale
targets, the cluster settings and the provenance.

    bench/appendix_a.py a1 a2 a3 a4 a5 --pg-config /usr/lib/postgresql/16/bin/pg_config --parent /var/tmp \\
        --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
    bench/appendix_a.py a1 --scale 1000 --extension-revision … --extension-profile debug   # a quick check

Each use case runs in its own database, dropped before the next, and
stops before a step if the disk has less than `--disk-floor` free. A stage
that fails ends its use case (after a ROLLBACK); a check that fails is
recorded and the use case goes on."""

import argparse
import json
import os
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent / "test"))

import soak  # noqa: E402
from harness.cluster import Cluster  # noqa: E402
from use_cases import CASES  # noqa: E402
from use_cases.draws import FUNCTIONS  # noqa: E402
from use_cases.plan import resolve  # noqa: E402

GIB = 1 << 30
DATABASE = "appendix_a"
STAGE_FIELDS = ("seconds", "median_seconds", "peak_rss_anon", "vm_hwm", "rss_anon_before", "rss_anon_after",
                "notices", "warnings")


def run_case(case, session, free_disk, floor):
    """Runs `case`'s steps in `session`: stages timed with their peaks,
    measures and checks recorded, values carried into later SQL."""
    values, result = {}, {"steps": [], "checks": [], "values": {}}
    for step in case.steps:
        free = free_disk()
        if free < floor:
            result["aborted"] = f"free disk {soak.size(free)} below the floor {soak.size(floor)} before {step.name}"
            break
        try:
            sql = resolve(step.sql, values)
        except KeyError as missing:
            result["aborted"] = f"{step.name} needs {missing}, which no step recorded"
            break
        if step.kind == "check":
            result["checks"].append(_check(step, session, sql))
            continue
        record, error = (_stage if step.kind == "stage" else _measure)(step, session, sql)
        record["free_disk_before"] = free
        result["steps"].append(record)
        if error:
            session.run("ROLLBACK")
            result["aborted"] = f"{step.name}: {error}"
            break
        if step.key:
            values[step.key] = record["value"]
    result["values"] = values
    result["verdicts"] = [{"stage": s["name"], "target_seconds": s["target_seconds"],
                           "median_seconds": s["median_seconds"], "met": s["met"]}
                          for s in result["steps"] if "met" in s]
    result["passed"] = sum(c["passed"] for c in result["checks"])
    result["failed"] = len(result["checks"]) - result["passed"]
    return result


def _stage(step, session, sql):
    stage = session.measure(sql, step.repeat)
    record = {"name": step.name, "kind": "stage", **{k: stage[k] for k in STAGE_FIELDS if k in stage}}
    errors = stage.get("errors")
    if errors:
        if step.allowed_error and any(step.allowed_error in e for e in errors):
            record["allowed_error"] = errors[0]
            return record, None
        record["errors"] = errors
        return record, errors[0]
    record["value"] = stage["first_rows"][0]
    if step.target is not None:
        record["target_seconds"] = step.target
        record["met"] = stage["median_seconds"] <= step.target
    return record, None


def _measure(step, session, sql):
    out = session.run(sql)
    record = {"name": step.name, "kind": "measure", "seconds": round(out["seconds"], 4),
              "value": out["rows"][-1] if out["rows"] else None}
    if out["errors"]:
        record["errors"] = out["errors"]
        return record, out["errors"][0]
    return record, None


def _check(step, session, sql):
    out = session.run(sql)
    if out["errors"]:
        session.run("ROLLBACK")
        return {"name": step.name, "claim": step.claim, "value": None, "passed": False, "error": out["errors"][0]}
    value = out["rows"][-1] if out["rows"] else None
    return {"name": step.name, "claim": step.claim, "value": value, "passed": bool(step.expect(value))}


def run(cluster, case, options, provenance):
    admin = cluster.database()
    admin.execute(f"DROP DATABASE IF EXISTS {DATABASE}")
    admin.execute(f"CREATE DATABASE {DATABASE}")
    result = {"provenance": provenance, "case": case.name, "section": case.section, "scale": options.scale,
              "sizes": case.sizes, "threads": options.threads, "hardware": soak.hardware(),
              "started_at": soak._now(), "free_disk_before": soak.free_disk(options.parent)}
    try:
        with soak.Session(cluster, DATABASE) as session:
            session.run("CREATE EXTENSION pg_warren")
            result["pg_warren"] = session.run("SELECT warren.version()")["rows"][0]
            for statement in (*FUNCTIONS, f"SET warren.threads = {options.threads}", *case.settings):
                session.run(statement)
            result["settings"] = session.run(soak.SETTINGS_SQL)["rows"]
            result.update(run_case(case, session, lambda: soak.free_disk(options.parent), options.disk_floor))
    except soak.BackendLost as lost:
        result["aborted"] = str(lost)
    finally:
        admin.execute(f"DROP DATABASE IF EXISTS {DATABASE}")
        result["finished_at"], result["load_average_after"] = soak._now(), os.getloadavg()
    return result


def report(result):
    for step in result.get("steps", []):
        verdict = "" if "met" not in step else (" met" if step["met"] else " MISS") + f" (< {step['target_seconds']} s)"
        print(f"  {step['name']}: {step.get('median_seconds', step.get('seconds'))} s{verdict}", flush=True)
    for check in result.get("checks", []):
        print(f"  {'pass' if check['passed'] else 'FAIL'} {check['name']}: {check['value']}"
              + (f" ({check['error']})" if "error" in check else ""), flush=True)
    if "aborted" in result:
        print(f"  ABORTED: {result['aborted']}", flush=True)


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("cases", nargs="*", help=f"use cases to run (default: all of {', '.join(CASES)})")
    options.add_argument("--pg-config", default=os.environ.get("PG_CONFIG", "pg_config"))
    options.add_argument("--extension-revision", required=True)
    options.add_argument("--extension-profile", required=True, choices=["release", "debug"])
    options.add_argument("--runas", default="postgres", help="cluster user when run as root")
    options.add_argument("--parent", type=Path, default=Path("/var/tmp"), help="where the cluster directory goes")
    options.add_argument("--results", type=Path, default=HERE / "results" / "appendix_a")
    options.add_argument("--scale", type=int, default=1, help="divide every size by this (quick checks)")
    options.add_argument("--threads", type=int, default=os.cpu_count())
    options.add_argument("--disk-floor", type=int, default=GIB, help="bytes of disk to keep free")
    options.add_argument("--label", default="", help="suffix for the result file name")
    args = options.parse_args(argv)
    args.cases = args.cases or list(CASES)
    if unknown := set(args.cases) - set(CASES):
        options.error(f"unknown use cases: {', '.join(sorted(unknown))}")
    args.results.mkdir(parents=True, exist_ok=True)
    found = soak.provenance([sys.argv[0], *argv], args.extension_revision, args.extension_profile,
                            soak.installed_library(args.pg_config))
    failed = False
    with Cluster(args.pg_config, args.runas, parent=args.parent, settings=soak.SETTINGS) as cluster:
        for name in args.cases:
            case = CASES[name](args.scale)
            print(f"{name} ({case.section}): {case.sizes}, free disk {soak.size(soak.free_disk(args.parent))}",
                  flush=True)
            result = run(cluster, case, args, found)
            suffix = (f"-scale{args.scale}" if args.scale > 1 else "") + args.label
            out = args.results / f"{name}{suffix}.json"
            out.write_text(json.dumps(result, indent=1) + "\n")
            report(result)
            print(f"  wrote {out}", flush=True)
            failed |= "aborted" in result or bool(result.get("failed"))
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
