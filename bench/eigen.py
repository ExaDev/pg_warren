#!/usr/bin/env python3
"""The ARPACK baseline for `warren.eigen` (DESIGN.md §6.5, §12): runs SciPy's
`eigsh` and warren-core's Lanczos (the `eigen_convergence` example) on the
same generated graph, and records operator applications, seconds and the
largest eigenvalue difference side by side, with the eigenvalue gaps that
decide how many steps the wanted end of the spectrum needs.

    bench/eigen.py                                  # random-100k, k = 20, tol 1e-6
    bench/eigen.py --nodes 2000 --threads 1         # quick check
    bench/eigen.py --graph sbm --communities 40 --cases normalized_laplacian:smallest
    bench/eigen.py --step-cost --nodes 1000000      # one step: SpMV against the rest

Graphs are undirected and unweighted with average degree 20 (10 edges per
node): the harness's `random` fixture, G(n, m), or its `sbm` fixture with
equal communities, 16 of the 20 edges of a node inside its community. The
matrix has two non-zeros per edge. Operators are taken over the nodes with an
edge, as warren does. Every timing records the load average before and after
(the machine is shared: steps are reliable, seconds are not). Writes
`results/eigen/<graph>.json` (`<graph>-step-cost.json` with `--step-cost`).
"""

import argparse
import json
import os
import platform
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np
import scipy.sparse as sp
from scipy.sparse.linalg import LinearOperator, eigsh

from fixtures import random_graph, sbm

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CASES = (("adjacency", "largest"), ("normalized_laplacian", "smallest"))
ARPACK_WHICH = {"largest": "LM", "smallest": "SA"}
DEGREE = 20
DEGREE_INSIDE = 16
GAP_EXTRA = 4


def graph_edges(kind, nodes, seed, communities=40):
    """The undirected edges `(src, dst)`, ids 1..nodes, of a `random` or
    `sbm` graph of average degree 20."""
    if kind == "random":
        table = random_graph.generate(nodes, DEGREE * nodes // 2, seed).table("edges")
    else:
        if nodes % communities:
            raise ValueError(f"{communities} communities do not divide {nodes} nodes")
        size = nodes // communities
        p_in = DEGREE_INSIDE / (size - 1)
        p_out = (DEGREE - DEGREE_INSIDE) / (nodes - size)
        table = sbm.generate(communities, size, p_in, p_out, seed).table("edges")
    return table.column("src"), table.column("dst")


def graph_name(kind, nodes, seed, communities):
    prefix = "random" if kind == "random" else f"sbm{communities}"
    return f"{prefix}-{nodes}-{DEGREE * nodes // 2}-seed{seed}"


def write_edges(path, src, dst):
    """`edges.csv` as the fixtures write it: a header, then `src,dst` rows."""
    with open(path, "w") as out:
        out.write("src,dst\n")
        np.savetxt(out, np.column_stack([src, dst]), fmt="%d", delimiter=",")


def operator_matrix(src, dst, nodes, operator):
    """The symmetric `adjacency` or `normalized_laplacian` of the undirected
    edges `src`–`dst` (ids 1..nodes), restricted to nodes with an edge."""
    rows = np.concatenate([src, dst]) - 1
    cols = np.concatenate([dst, src]) - 1
    a = sp.csr_array((np.ones(len(rows)), (rows, cols)), shape=(nodes, nodes))
    a.data[:] = 1.0  # duplicates fold to one edge, as the CSR build does
    degree = np.asarray(a.sum(axis=1)).ravel()
    active = np.flatnonzero(degree > 0)
    a = a[active][:, active]
    if operator == "adjacency":
        return a.tocsr()
    scale = sp.diags_array(1.0 / np.sqrt(degree[active]))
    return (sp.eye_array(len(active)) - scale @ a @ scale).tocsr()


def ordered(values, which):
    values = np.asarray(values)
    return values[np.argsort(-np.abs(values))] if which == "largest" else np.sort(values)


def arpack(matrix, k, which, tol, seed=1):
    """`eigsh` with the default basis size and a seeded start vector,
    counting operator applications."""
    matvecs = 0

    def apply(x):
        nonlocal matvecs
        matvecs += 1
        return matrix @ x

    operator = LinearOperator(matrix.shape, matvec=apply, dtype=np.float64)
    load, start = os.getloadavg()[0], time.perf_counter()
    v0 = np.random.default_rng(seed).uniform(-1.0, 1.0, matrix.shape[0])
    values = eigsh(operator, k=k, which=ARPACK_WHICH[which], tol=tol, v0=v0, return_eigenvectors=False)
    seconds = time.perf_counter() - start
    return {"matvecs": matvecs, "seconds": round(seconds, 3), "load_average": [load, os.getloadavg()[0]],
            "eigenvalues": ordered(values, which).tolist()}


def spread(matrix):
    """`λ_max − λ_min`, the width of the spectrum a Krylov method sees."""
    # Several values at each end, from a seeded start: with k = 1, ARPACK
    # can settle on the second value of a crowded end (a path's).
    n = matrix.shape[0]
    v0 = np.random.default_rng(1).uniform(-1.0, 1.0, n)
    top, bottom = (eigsh(matrix, k=min(4, n - 1), which=w, tol=1e-12, ncv=min(n, 40), v0=v0,
                         return_eigenvectors=False) for w in ("LA", "SA"))
    return float(top.max() - bottom.min())


def gaps(values, k, which, spread):
    """How crowded the wanted end is: `next_gap` between the k-th value and
    the first unwanted one (in magnitude for `largest`), the mean spacing of
    the wanted values after the first, and the next gap relative to the
    spectrum's width. `values` holds more than `k` values, in `which` order."""
    keys = np.abs(values) if which == "largest" else np.asarray(values, dtype=float)
    next_gap = float(abs(keys[k] - keys[k - 1]))
    mean = float(abs(keys[k - 1] - keys[1]) / (k - 2)) if k > 2 else None
    return {"next_gap": next_gap, "mean_spacing": mean, "spread": spread, "relative_next_gap": next_gap / spread}


def measure_gaps(matrix, k, which):
    """`gaps` from a tight `eigsh` of `k + 4` values."""
    values = eigsh(matrix, k=k + GAP_EXTRA, which=ARPACK_WHICH[which], tol=1e-10, return_eigenvectors=False)
    return gaps(ordered(values, which), k, which, spread(matrix))


def warren_command(edges_csv, nodes, operator, which, k, tol, threads, precision, max_iter):
    command = ["cargo", "run", "--quiet", "--release", "-p", "warren-core", "--example", "eigen_convergence",
               "--", str(edges_csv), str(nodes), operator, which, str(k), str(tol), str(threads), precision]
    return command + ([str(max_iter)] if max_iter is not None else [])


def warren(*args):
    """One run of the `eigen_convergence` example (built in release)."""
    load = os.getloadavg()[0]
    done = subprocess.run(warren_command(*args), cwd=ROOT, capture_output=True, text=True, check=True)
    return {**json.loads(done.stdout), "load_average": [load, os.getloadavg()[0]]}


def step_cost(edges_csv, nodes, operator, which, k, threads, precision, steps):
    """One run of the `eigen_step_cost` example: seconds per step, per SpMV
    and the rest."""
    command = ["cargo", "run", "--quiet", "--release", "-p", "warren-core", "--example", "eigen_step_cost",
               "--", str(edges_csv), str(nodes), operator, which, str(k), str(threads), precision, str(steps)]
    load = os.getloadavg()[0]
    done = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    return {**json.loads(done.stdout), "load_average": [load, os.getloadavg()[0]]}


def machine():
    model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                  if line.startswith("model name")), platform.processor())
    memory = os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30
    return {"cpus": os.cpu_count(), "cpu": model, "memory_gib": round(memory, 1)}


def parse(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("--graph", choices=("random", "sbm"), default="random")
    options.add_argument("--nodes", type=int, default=100_000)
    options.add_argument("--communities", type=int, default=40, help="sbm only")
    options.add_argument("--seed", type=int, default=1)
    options.add_argument("--k", type=int, default=20)
    options.add_argument("--tol", type=float, default=1e-6)
    options.add_argument("--threads", default="1,4", help="warren thread counts, comma-separated")
    options.add_argument("--precision", default="f32", help="f32, f64 or both, comma-separated")
    options.add_argument("--max-iter", type=int, default=None, help="warren's max_iter (default: its own)")
    options.add_argument("--cases", default=",".join(f"{o}:{w}" for o, w in CASES),
                         help="operator:which pairs, comma-separated")
    options.add_argument("--no-arpack", action="store_true", help="skip the ARPACK baseline")
    options.add_argument("--no-gaps", action="store_true", help="skip the k + 4 solve that measures the gaps")
    options.add_argument("--step-cost", action="store_true",
                         help="time one step (SpMV against the rest) instead of solving")
    options.add_argument("--steps", type=int, default=200, help="--step-cost: steps per sample")
    options.add_argument("--results", type=Path, default=HERE / "results" / "eigen")
    return options.parse_args(argv)


def time_steps(args, edges_csv, operator, which):
    runs = {}
    for precision in args.precision.split(","):
        for threads in args.threads.split(","):
            run = step_cost(edges_csv, args.nodes, operator, which, args.k, threads, precision, args.steps)
            runs[f"{precision}_threads_{threads}"] = run
            print(f"{operator} {precision} threads {threads}: step {run['step_seconds']:.4f} s, "
                  f"spmv {run['spmv_seconds']:.4f} s, load {run['load_average']}", flush=True)
    return runs


def solve(args, edges_csv, src, dst, operator, which):
    matrix = operator_matrix(src, dst, args.nodes, operator)
    case = {"nnz": matrix.nnz, "warren": {}}
    if not args.no_gaps:
        case["gaps"] = measure_gaps(matrix, args.k, which)
        print(f"{operator} {which}: gaps {case['gaps']}", flush=True)
    baseline = None
    if not args.no_arpack:
        baseline = case["arpack"] = arpack(matrix, args.k, which, args.tol)
        print(f"{operator} {which}: arpack {baseline['matvecs']} matvecs, {baseline['seconds']} s", flush=True)
    del matrix
    for precision in args.precision.split(","):
        for threads in args.threads.split(","):
            ours = warren(edges_csv, args.nodes, operator, which, args.k, args.tol, threads, precision,
                          args.max_iter)
            if baseline:
                difference = np.max(np.abs(np.array(ours["eigenvalues"]) - baseline["eigenvalues"]))
                ours["max_eigenvalue_difference"] = float(difference)
            case["warren"][f"{precision}_threads_{threads}"] = ours
            print(f"  warren {precision} threads {threads}: {ours['steps']} steps, {ours['seconds']} s, "
                  f"converged {ours['converged']}", flush=True)
    return case


def main(argv):
    args = parse(argv)
    name = graph_name(args.graph, args.nodes, args.seed, args.communities)
    revision = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True)
    report = {"graph": name, "k": args.k, "tol": args.tol, "precision": args.precision,
              "max_iter": args.max_iter, "git": revision.stdout.strip() or None, "machine": machine(),
              "cases": []}
    with tempfile.TemporaryDirectory(prefix="warren-eigen-", dir="/var/tmp") as scratch:
        src, dst = graph_edges(args.graph, args.nodes, args.seed, args.communities)
        edges_csv = Path(scratch) / "edges.csv"
        write_edges(edges_csv, src, dst)
        for operator, which in (case.split(":") for case in args.cases.split(",")):
            case = {"operator": operator, "which": which}
            if args.step_cost:
                case["warren"] = time_steps(args, edges_csv, operator, which)
            else:
                case.update(solve(args, edges_csv, src, dst, operator, which))
            report["cases"].append(case)
    args.results.mkdir(parents=True, exist_ok=True)
    out = args.results / f"{name}{'-step-cost' if args.step_cost else ''}.json"
    out.write_text(json.dumps(report, indent=1) + "\n")
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
