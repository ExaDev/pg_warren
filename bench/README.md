# bench/

End-to-end benchmark harness and fixture generators (DESIGN.md §12,
"Performance"; Appendix A fixtures). How to run it is in CONTRIBUTING.md.

- `generate.py` — seeded, deterministic fixture generators: random
  (Erdős–Rényi), power-law (preferential attachment), grid, SBM, evolving
  graphs (SBM base + volume / bridge / novel ingest, A.2), and the Appendix A
  fixtures: `fraud` (A.1), `entity-resolution` (A.3), `citation` (A.4),
  `embedding` (A.5). Each writes CSVs, `load.sql` and the ground truth
  (`truth.json`, `truth_*` tables). The generators live in `fixtures/`, their
  pytest tests in `tests/`.
- `run.sh` / `matrix.py` — load each graph of `matrix.json`, run its setup and
  time every workload; workloads whose `requires` functions do not exist yet
  are recorded as skipped, so a new warren function slots in as one more
  matrix entry. Writes `results/<pg_warren version>/<graph>.json`.
- `eigen.py` — the ARPACK baseline for `warren.eigen`: SciPy `eigsh` and
  warren-core's Lanczos (`examples/eigen_convergence.rs`) on the same
  `random` fixture, operator applications and seconds side by side, written to
  `results/eigen/`.
- `results/` — committed results, one directory per pg_warren version
  (record them on a quiet machine; say which in the commit).
- `requirements.txt` — pinned Python dependencies of `bench/` and `test/`.

Micro-benchmarks of `warren-core` kernels live with the crate
(`cargo bench -p warren-core`, criterion).
