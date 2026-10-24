"""The suite's results as Markdown tables, for bench/results/corpus/README.md
and DESIGN.md §12: correctness a row a dataset, and timings a row a dataset
with a column a stage (`suite.py summary`)."""

SECTION_12 = ("refresh", "cold load", "bfs depth 6", "bfs depth 6 recursive CTE", "reachable_set", "components",
              "pagerank 20 iterations", "eigen k=20", "aggregate_neighbours d=64", "diff")
LIBRARY = ("neighbours", "triangles", "clustering", "core_number", "louvain", "leiden")


def _row(cells):
    return "| " + " | ".join(str(c) for c in cells) + " |"


def correctness(results):
    lines = [_row(["Dataset", "Tier", "Nodes", "Stored edges", "Checks matched", "Not matched",
                   "Q: louvain / leiden / NetworkX Louvain", "Seconds"]), _row(["---"] * 8)]
    for result in results:
        findings = result["findings"]
        matched = sum(f["status"] == "match" for f in findings)
        others = ", ".join(f"{f['check']} ({f['status']})" for f in findings if f["status"] != "match")
        by_check = {f["check"]: f for f in findings}
        q = (" / ".join(f"{by_check[c]['warren']:.3f}" for c in ("louvain", "leiden")) +
             f" / {by_check['louvain']['oracle']:.3f}") if "louvain" in by_check else ""
        version = result["version"]
        lines.append(_row([result["dataset"], result["tier"], version["node_count"], version["edge_count"],
                           f"{matched} of {len(findings)}", others, q, result["seconds"]]))
    return "\n".join(lines)


def seconds(stage):
    errors = " ".join(stage.get("errors", []))
    if "statement timeout" in errors:
        return "timeout"
    if errors:
        return "error"
    value = stage.get("server_median_seconds", stage["median_seconds"])
    return f"{value:.3f}" if value < 1 else f"{value:.2f}"


def timings(results, stages):
    lines = [_row(["Dataset", *stages]), _row(["---"] * (len(stages) + 1))]
    for result in results:
        lines.append(_row([result["dataset"], *(seconds(result["stages"][s]) if s in result["stages"] else "—"
                                                for s in stages)]))
    return "\n".join(lines)
