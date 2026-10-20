# 100M-edge soak results

`bench/soak.py` output, one file per case and run (DESIGN.md §12 "Measured").
Runs from now on record their own provenance under `provenance`: the full
argv, the harness's revision (and whether `bench/` or `test/` differed from
it), and the extension's revision and build profile as given to the
script, with the sha256 of the `pg_warren.so` the cluster loaded.

The five files below predate that. They were recorded on PostgreSQL 16.15
with release builds of pg_warren 0.3.0, at these revisions (from the runs
themselves; a `build` key added to each file by hand was removed):

| File | pg_warren build | Notes |
|---|---|---|
| `a.json` | 6e20074 | before both memory fixes; an earlier harness (no `skip` or `statement_timeout` keys) |
| `a-rerun.json` | 495646e | run without `eigen` (`--no-eigen`, which the harness did not record) |
| `b-unmerged-before-cache-fix.json` | f1987a1 | after the chunk-reader fix, before the held-version fix; an earlier harness; `eigen` cancelled at 769 s (`canceling statement due to user request`) |
| `b-unmerged.json` | 495646e | `--eigen-max-iter 200 --statement-timeout 15min` |
| `b-merged.json` | 495646e | `--eigen-max-iter 200 --statement-timeout 15min --skip second-version` |

Their harness revisions and exact command lines were not recorded. Their
verdicts are as that harness computed them: it judged `eigen` on time
alone (`a.json`'s 65-minute solve did not converge, residual 1.0·10⁻⁴; it
is a miss either way), left out stages that failed other than by a cancel,
and took the settings snapshot before the query sessions' `SET`s (so it
shows `warren.threads=2`, the default, where the queries ran with 4).
