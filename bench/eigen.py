#!/usr/bin/env python3
"""The ARPACK baseline for `warren.eigen` (DESIGN.md §6.5, §12): runs SciPy's
`eigsh` and warren-core's Lanczos (the `eigen_convergence` example) on the
same generated graph, and records operator applications, seconds and the
largest eigenvalue difference side by side.

    bench/eigen.py                                  # random-100k, k = 20, tol 1e-6
    bench/eigen.py --nodes 2000 --edges 10000 --threads 1   # quick check

The graph is the harness's `random` fixture (undirected, unweighted), so the
matrix has two non-zeros per edge. Operators are taken over the nodes with
an edge, as warren does. Writes `results/eigen/<graph>.json`.
"""

import argparse
import json
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import numpy as np
import scipy.sparse as sp
from scipy.sparse.linalg import LinearOperator, eigsh

from fixtures import random_graph

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
CASES = (("adjacency", "largest"), ("normalized_laplacian", "smallest"))
ARPACK_WHICH = {"largest": "LM", "smallest": "SA"}


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


def arpack(matrix, k, which, tol):
    """`eigsh` with the default basis size, counting operator applications."""
    matvecs = 0

    def apply(x):
        nonlocal matvecs
        matvecs += 1
        return matrix @ x

    operator = LinearOperator(matrix.shape, matvec=apply, dtype=np.float64)
    start = time.perf_counter()
    values = eigsh(operator, k=k, which=ARPACK_WHICH[which], tol=tol, return_eigenvectors=False)
    seconds = time.perf_counter() - start
    order = np.argsort(-np.abs(values)) if which == "largest" else np.argsort(values)
    return {"matvecs": matvecs, "seconds": round(seconds, 3), "eigenvalues": values[order].tolist()}


def warren(edges_csv, nodes, operator, which, k, tol, threads, precision):
    """One run of the `eigen_convergence` example (built in release)."""
    command = ["cargo", "run", "--quiet", "--release", "-p", "warren-core", "--example", "eigen_convergence",
               "--", str(edges_csv), str(nodes), operator, which, str(k), str(tol), str(threads), precision]
    done = subprocess.run(command, cwd=ROOT, capture_output=True, text=True, check=True)
    return json.loads(done.stdout)


def main(argv):
    options = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    options.add_argument("--nodes", type=int, default=100_000)
    options.add_argument("--edges", type=int, default=1_000_000)
    options.add_argument("--seed", type=int, default=1)
    options.add_argument("--k", type=int, default=20)
    options.add_argument("--tol", type=float, default=1e-6)
    options.add_argument("--threads", default="1,4", help="warren thread counts, comma-separated")
    options.add_argument("--precision", default="f32", choices=("f32", "f64"))
    options.add_argument("--results", type=Path, default=HERE / "results" / "eigen")
    args = options.parse_args(argv)
    name = f"random-{args.nodes}-{args.edges}-seed{args.seed}"
    report = {"graph": name, "k": args.k, "tol": args.tol, "precision": args.precision, "cases": []}
    with tempfile.TemporaryDirectory(prefix="warren-eigen-") as scratch:
        random_graph.generate(args.nodes, args.edges, args.seed).write(Path(scratch))
        edges = np.loadtxt(Path(scratch) / "edges.csv", delimiter=",", skiprows=1, dtype=np.int64, ndmin=2)
        for operator, which in CASES:
            matrix = operator_matrix(edges[:, 0], edges[:, 1], args.nodes, operator)
            baseline = arpack(matrix, args.k, which, args.tol)
            case = {"operator": operator, "which": which, "nnz": matrix.nnz, "arpack": baseline, "warren": {}}
            print(f"{operator} {which}: arpack {baseline['matvecs']} matvecs, {baseline['seconds']} s", flush=True)
            for threads in args.threads.split(","):
                ours = warren(Path(scratch) / "edges.csv", args.nodes, operator, which, args.k, args.tol,
                              threads, args.precision)
                difference = np.max(np.abs(np.array(ours["eigenvalues"]) - baseline["eigenvalues"]))
                ours["max_eigenvalue_difference"] = float(difference)
                case["warren"][f"threads_{threads}"] = ours
                print(f"  warren threads {threads}: {ours['steps']} steps, {ours['seconds']} s, "
                      f"|Δλ| ≤ {difference:.1e}", flush=True)
            report["cases"].append(case)
    args.results.mkdir(parents=True, exist_ok=True)
    out = args.results / f"{name}.json"
    out.write_text(json.dumps(report, indent=1) + "\n")
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
