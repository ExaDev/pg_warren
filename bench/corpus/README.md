# bench/corpus/

The real-graph suite (DESIGN.md §12, §13 M11): datasets from the
[keepset](https://mearman.github.io/keepset/) catalogue, fetched by keepset's
own recipes, pinned by checksum, loaded into the `nodes` and `edges` tables
`warren.create_graph` reads, checked against NetworkX and timed on the §12
matrix. Raw data never enters git: it lives in a cache (default
`/var/tmp/warren-corpus`, `--cache` or `$WARREN_CORPUS_CACHE`).

```sh
bench/corpus/suite.py fetch --tier tiny                 # fetch, pin and convert to tables
bench/corpus/suite.py check --tier tiny --pg-config $T/usr/lib/postgresql/16/bin/pg_config \
    --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
bench/corpus/suite.py time --tier medium --pg-config ... --extension-revision ... --extension-profile release
bench/corpus/suite.py ids --tier tiny --tier medium     # text id lengths and front coding
```

`check` and `time` start their own scratch cluster from `--pg-config`'s
binaries and load the `pg_warren` installed there: use a private tree with a
release build, as CONTRIBUTING.md describes for the soak, so nothing touches
the shared install (no `/tmp/pgrx-test.lock` is needed for a private tree).
Results go to `bench/results/corpus/` (`--results`): `correctness/<dataset>.json`,
`<dataset>.json` for timings and `ids/<dataset>.json`, each with its
provenance (the command, the harness's revision, the extension's revision,
profile and library sha256, and keepset's provenance of the bytes).

## Datasets and tiers

`manifest.json` lists each dataset by its keepset id with its tier, domain,
id kind, direction, weights, relations, licence as keepset records it, and
why it is in the suite; `pins.json` holds the sha256, size and URL of the
bytes each was first fetched from, written by the first `fetch`. A later
fetch whose bytes differ fails (`PinMismatch`) and leaves nothing behind.

| Tier | Datasets | Fetch | `check` | `time` |
|---|---|---|---|---|
| tiny (CI-able) | karate, lesmis, celegans, polblogs, london_transport, transport-airline, facebook, kg-codex-s, wn18rr | 25 s, 9 MB of tables | about a minute | not run |
| medium (about 1M edges) | cit-hepth, email-enron, fb15k-237, ss-gleich-usroads-48, dblp, yago3-10, hetionet, roadnet-tx | see results | see results | see results |
| large | livejournal, ogbn-mag | needs about 75 and 45 GB of Node heap (below) | not run | not run |

## Pinning

- **keepset** itself: the CLI is installed with `npm ci` from
  `keepset_cli/package-lock.json` (keepset 0.57.0, commit 944b86e, catalogue
  1.33.0, and every dependency by integrity hash) into the cache.
- **the bytes**: keepset records the sha256 of what it downloaded and from
  where (`<name>.provenance.json`); `fetch` checks it against `pins.json`.
  keepset reads a source's pinned Wayback snapshot first, then the closest
  capture, then the live URL; the pin is of the bytes, wherever they came from.
  `--no-archive` is always passed, so a fetch never asks the Internet Archive
  to capture a URL.
- **the shape**: `tables.py` refuses a document whose direction or id kind
  disagrees with the manifest, a weighted dataset whose edges carry no weight,
  and relations the manifest does not expect. Ids are `bigint` only when every
  one is a canonical int8 (WN18RR's `00260881` is text).

## Correctness (`check`)

For each dataset, graph `g` is built as the manifest describes it and compared
with NetworkX on the same tables (`oracle.py`, under pg_warren's definitions:
deduplicated edges, neighbours counted once, the undirected simple graph for
triangles, clustering and cores, the undirected view for modularity):
the version's node count and dropped edges; degree (out, and in and both when
directed); BFS depths from the busiest node and a seeded random one;
components (as partitions); shortest path lengths and validity for 25 seeded
pairs, and Dijkstra costs when weighted; PageRank (within 1e-9, both converged
to 1e-12); triangles; clustering (within 1e-12); core numbers;
`warren.modularity` against NetworkX's on NetworkX's Louvain partition; and
`louvain` and `leiden` held to a quality bound (at least NetworkX Louvain's
modularity less 0.02) with the modularity they return checked against
NetworkX's of their partition.

## A CI job (proposed, not added)

The tiny tier fits CI's time: about 25 s to fetch, a minute to check, 9 MB of
tables. It needs network access to the datasets' sources (SNAP, Newman's
netdata, KONECT, Netzschleuder, the KGE mirror), which CI has, and Node 22 for
keepset. A job would: install Node, `npm ci` keepset (cached on the lock's
hash), cache the tables directory on `pins.json`'s hash so most runs fetch
nothing, build the release library into a private tree, then run
`suite.py check --tier tiny`. Not added because CI would then depend on
third-party hosts (several were unreachable while this was written: LINQS for
Cora and CiteSeer, GroupLens for MovieLens, and the Wayback Machine for every
snapshot), and a failed fetch should not fail main; a scheduled (nightly) job
that reports rather than gates is the better fit.

## The large tier

Not run yet (other agents share the machine, so timings would be noisy). Its
cost is keepset's fetch: keepset 0.57.0 holds the whole graph/v1 document and
its crosswalk copies every edge (ExaDev/crossmap#57; keepset marks wikidata5m
and ogbl-wikikg2 not fetchable for the same reason), measured at 7.8 GB
resident for roadnet-tx's 3.84M edges, about 2.0 kB an edge.
`keepset.heap_mb` sizes Node's heap from that: about 74 GB for LiveJournal's
34.7M edges and 45 GB for ogbn-mag's 21.1M, more than this 15 GB machine. The
rest of the pipeline streams (`graph_v1.py` reads the document a node and an
edge at a time). DESIGN.md §12 has the estimates for the database side.
