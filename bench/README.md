# bench/

End-to-end benchmark harness (DESIGN.md §12, "Performance"). Planned contents:

- `generate.py` — fixture generators: random, power-law (preferential
  attachment), grid, and evolving graphs (base + parameterised perturbation).
- `run.sh` — loads fixtures into `nodes`/`edges`, builds, and runs the workload
  matrix against baselines (recursive CTE, NetworkX, igraph).
- `results/` — committed results, one file per pg_warren version.

Micro-benchmarks of `warren-core` kernels live with the crate
(`cargo bench -p warren-core`, criterion). Status: stub — nothing here yet.
