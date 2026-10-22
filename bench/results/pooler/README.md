# Pooler results

`bench/pooler.py` output, one file per case (DESIGN.md §12, "The shared
cache"). Each records its provenance: the argv, the harness's revision,
and the extension's revision, build profile and `pg_warren.so` sha256.

`a-scale10.json` and `b-unmerged-scale10.json`: cases a and b-unmerged at
a tenth of the soak's scale (1M nodes, 10M edges), own graphs at a
hundredth, run one after the other on 2026-10-22 on the soak's 4-vCPU
Xeon (15 GiB RAM, no swap), PostgreSQL 16.15 from a private tree with a
release build of e32dd67 installed:

    bench/pooler.py a b-unmerged --own-scale 10 \
        --pg-config /var/tmp/m9-pgtree/usr/lib/postgresql/16/bin/pg_config \
        --extension-revision e32dd67 --extension-profile release

The harness was feef7c5. It is recorded as dirty only because the
working tree held this directory's earlier, uncommitted results, which
these replace (that run's `reset` evicted only the scenario's own graphs,
so its own-graph runs began with case a's shared version still held).

Each run's `cold_first_calls` holds every session's server-side seconds.
Several runs of the own-graph scenario alone (shared mode first, and off
first) gave 50 cold first reads of b-unmerged's 34 MB versions a median of
0.24–3.85 s shared and 0.31–0.93 s off. Those runs were not kept.
DESIGN.md §12 records this as open.
