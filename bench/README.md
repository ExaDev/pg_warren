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
- `soak.py` — the 100M-edge soak (DESIGN.md §12, §13 M7): 10M nodes and 100M
  edges generated inside a scratch cluster, built and queried at the default
  limits, with each backend's peak memory from `/proc`; verdicts against the
  §12 targets in `results/soak/<case>.json`. Run it against a Postgres tree
  with a release build installed (`--pg-config`), on a machine with ~15 GB of
  RAM and ~20 GB of free disk; `--scale 1000` is a quick check.
- `results/` — committed results, one directory per pg_warren version
  (record them on a quiet machine; say which in the commit).
- `requirements.txt` — pinned Python dependencies of `bench/` and `test/`.

Micro-benchmarks of `warren-core` kernels live with the crate
(`cargo bench -p warren-core`, criterion).
