# Parallel results

`bench/parallel.py` output (DESIGN.md §12, "Parallel query"; §5.6 step 9).
Each file records its provenance: the argv, the harness's revision, and
the extension's revision, build profile and `pg_warren.so` sha256.

Both runs measured the build of cf9dce1 (main before step 9, its graph
reads still `PARALLEL RESTRICTED`); the `safe` label is set by `ALTER
FUNCTION` in the benchmark's database, so `restricted` and `safe` are the
same library. The harness is recorded as dirty because `bench/parallel.py`
itself was not yet committed. On 2026-10-22 on the soak's 4-vCPU Xeon
(15 GiB RAM, no swap), PostgreSQL 16.15 from a private tree with the
release build installed:

    bench/parallel.py --pg-config /var/tmp/m9par-pgtree/usr/lib/postgresql/16/bin/pg_config \
        --extension-revision cf9dce1 --extension-profile release --scale 10 \
        --rows 10000 100000 1000000                                    # a-scale10.json
    bench/parallel.py … --scale 1 --rows 1000000 --workloads has_edge --repeat 2   # a.json

`a-scale10.json`: case a at a tenth of the soak's scale (1M nodes, 10M
edges, a 112 MB version), three workloads, probe tables of 10k, 100k and 1M
rows, three runs each. `a.json`: full scale (10M nodes, 100M edges, a
1.12 GB version), `has_edge` over 1M rows, two runs each (the first of
the `private` mode's runs includes reading the segments from disk).
`private` is PG 16 without the preload, where every worker loads its own
copy.
