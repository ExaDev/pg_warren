"""The head-to-head's results as Markdown (`suite.py summary`): what each
engine took to load each dataset, and each workload cold / warm (the warm
median) a cell an engine. A cell shows a time only when the engine's
answer agreed with the reference; otherwise it shows why not: the check's
status (`mismatch`, `below bound`), `timeout`, `error`, or `—` when the
engine cannot run the workload (the reasons are listed)."""

ENGINES = ("pg_warren", "pggraph", "igraph", "neo4j-cypher", "neo4j-gds")


def seconds(value):
    if value < 0.001:
        return f"{value * 1e6:.0f} µs"
    if value < 0.01:
        return f"{value * 1000:.2f} ms"
    if value < 1:
        return f"{value:.3f}"
    return f"{value:.2f}" if value < 100 else f"{value:.0f}"


def _row(cells):
    return "| " + " | ".join(str(c) for c in cells) + " |"


def cell(measured):
    if measured is None:
        return ""
    if measured.get("counted"):
        return f"{seconds(measured['cold_seconds'])} / {seconds(measured['warm_median_seconds'])}"
    if "check" in measured:
        return measured["check"]["status"]
    return "—" if measured["status"] == "unsupported" else measured["status"]


def workloads(results, engines=ENGINES):
    names = [e for e in engines if any(e in r["engines"] for r in results)]
    lines = [_row(["Dataset", "Workload", *names]), _row(["---"] * (len(names) + 2))]
    for result in results:
        ran = result["engines"]
        order = list(dict.fromkeys(w for e in names if e in ran for w in ran[e]["workloads"]))
        for workload in order:
            lines.append(_row([result["dataset"], workload,
                               *(cell(ran.get(e, {}).get("workloads", {}).get(workload)) for e in names)]))
    return "\n".join(lines)


def loads(results, engines=ENGINES):
    """Each engine's load phases, medians, a row a dataset and engine."""
    lines = [_row(["Dataset", "Engine", "Phases (median seconds)"]), _row(["---"] * 3)]
    for result in results:
        for engine in engines:
            if engine in result["engines"]:
                phases = result["engines"][engine]["load"]
                lines.append(_row([result["dataset"], engine, ", ".join(
                    f"{phase} {seconds(p['median_seconds'])}" for phase, p in phases.items())]))
    return "\n".join(lines)


def unsupported(results, engines=ENGINES):
    """Each (engine, workload) an engine could not run, with its reason, once."""
    reasons = {}
    for result in results:
        for engine in engines:
            for workload, measured in result["engines"].get(engine, {}).get("workloads", {}).items():
                if measured.get("status") == "unsupported":
                    reasons.setdefault((engine, workload), measured["reason"])
    return "\n".join(f"- {engine}, {workload}: {reason}" for (engine, workload), reason in reasons.items())
