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
  warren-core's Lanczos (`examples/eigen_convergence.rs`) on the same graph
  (`--graph random`, G(n, m), or `--graph sbm`, equal communities; both of
  average degree 20), operator applications and seconds side by side, with
  the eigenvalue gaps of the wanted end (`gaps`: λ_{k+1} − λ_k from a k + 4
  solve, the mean spacing, the spectrum's width) and the load average of
  every timing, written to `results/eigen/<graph>.json`. `--step-cost`
  instead splits one Lanczos step into its SpMV and the rest
  (`examples/eigen_step_cost.rs`), per precision and thread count
  (`results/eigen/<graph>-step-cost.json`). DESIGN.md §6.5 and §14 item 21
  read these files; the READMEs in `results/eigen/scaling/` and
  `results/eigen/step-cost/` say which build and machine each came from.
- `soak.py` — the 100M-edge soak (DESIGN.md §12, §13 M7): 10M nodes and 100M
  edges generated inside a scratch cluster, built and queried at the default
  limits, with each backend's peak memory from `/proc`; verdicts against the
  §12 targets in `results/soak/<case>.json`. Run it against a Postgres tree
  with a release build installed (`--pg-config`), passing the revision and
  profile it was built from (`--extension-revision`, `--extension-profile`;
  recorded with the results), on a machine with ~15 GB of RAM and ~20 GB of
  free disk; `--scale 1000` is a quick check. Since 0.4.0 each case also
  times the 0.4.0 library and `louvain`/`leiden` (`--skip library`,
  `--skip communities`), and `--node-order locality` builds in locality
  order.
- `appendix_a.py` and `use_cases/` — DESIGN.md Appendix A's use cases
  (A.1–A.5) at about 10M nodes (DESIGN.md §12, "Appendix A at 10M"; §13
  M11). Each use case in `use_cases/` is a plan of steps: it generates its
  planted structure inside a scratch cluster (deterministic SQL draws,
  `use_cases/draws.py`), runs Appendix A's program in timed stages, each
  with the backend's peak memory (`soak.py`'s session), and checks the
  expected outcomes that can be checked at that scale (planted structure
  recovered, determinism, invariants, an oracle on a sample). Results,
  with verdicts against Appendix A's scale targets, go to
  `results/appendix_a/<case>.json`; its README has the sizes and what
  each case checks. Same build and machine requirements as `soak.py`
  (~8 GB of free disk is enough: each use case runs in its own database,
  dropped before the next); `--scale 1000` is a quick check.
- `corpus/` — the real-graph suite (DESIGN.md §12, §13 M11): keepset datasets
  in tiers (tiny, medium, large), fetched by keepset's recipes and pinned by
  checksum (`manifest.json`, `pins.json`), loaded into `nodes`/`edges`,
  checked against NetworkX (`suite.py check`), timed on the §12 matrix
  (`suite.py time`) and their text ids measured for front coding
  (`suite.py ids`); results in `results/corpus/`. Its README has the details.
- `results/` — committed results, one directory per pg_warren version
  (record them on a quiet machine; say which in the commit).
- `requirements.txt` — pinned Python dependencies of `bench/` and `test/`.

Micro-benchmarks of `warren-core` kernels live with the crate
(`cargo bench -p warren-core`, criterion).
