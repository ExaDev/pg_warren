# bench/corpus/

The real-graph suite (DESIGN.md §12, §13 M11): datasets from the
[keepset](https://mearman.github.io/keepset/) catalogue, fetched by keepset's
own recipes, pinned by checksum, loaded into the `nodes` and `edges` tables
`warren.create_graph` reads, checked against NetworkX and igraph and timed on
the §12 matrix. Raw data never enters git: it lives in a cache (default
`/var/tmp/warren-corpus`, `--cache` or `$WARREN_CORPUS_CACHE`).

```sh
bench/corpus/suite.py fetch --tier tiny                 # fetch, pin and convert to tables
bench/corpus/suite.py check --tier tiny --pg-config $T/usr/lib/postgresql/16/bin/pg_config \
    --extension-revision "$(git rev-parse --short HEAD)" --extension-profile release
bench/corpus/suite.py check --tier medium --oracle igraph --pg-config ... # one oracle only
bench/corpus/suite.py time --tier medium --pg-config ... --extension-revision ... --extension-profile release
bench/corpus/suite.py ids --tier tiny --tier medium     # text id lengths and front coding
bench/corpus/suite.py head-to-head facebook dblp --pg-config ... --extension-revision ... --extension-profile release
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
| tiny (nightly in CI) | karate, lesmis, celegans, polblogs, london_transport, transport-airline, facebook, kg-codex-s, wn18rr | 25 s, 9 MB of tables | about 3 minutes | not run |
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
with each oracle on the same tables. `oracle/` is a port: `Source` reads the
tables once under pg_warren's definitions (deduplicated edges, neighbours
counted once, the undirected simple graph for triangles, clustering and cores,
the undirected view for modularity), and each library's reference
(`NetworkXReference`, `IGraphReference`: NetworkX 3.7 and python-igraph 1.0.0,
pinned in `bench/requirements.txt`) answers the same questions from it, so
`checks.py` does not know which library answered. pg_warren is asked once per
check and its answer compared with every oracle's; each finding records what
it was checked `against` (`--oracle` picks them; by default both). igraph is
independent of NetworkX (its C core) and much faster: with igraph alone a
medium dataset's check took 5–88 s, against 19–777 s with NetworkX alone
(`--oracle igraph`). The checks:
the version's node count and dropped edges; degree (out, and in and both when
directed); BFS depths from the busiest node and a seeded random one;
components (as partitions); shortest path lengths and validity for 25 seeded
pairs, and Dijkstra costs when weighted; PageRank (within 1e-9, both converged
to 1e-12; against igraph, which solves for the fixed point, within that plus
N·tol·α/(1 − α), how far an iteration that stops at an L1 change of N·tol
can be from it); triangles; clustering (within 1e-12); core numbers; exact
normalised betweenness (within 1e-12) where the oracle can afford it
(N·M up to 10⁸ for NetworkX, 5·10⁹ for igraph: every tiny graph but
transport-airline, facebook and WN18RR for NetworkX, all of them and
FB15k-237 for igraph); `warren.modularity` against the oracle's on the
oracle's Louvain partition (and per relation where there are relations); and
`louvain` and `leiden` held to a quality bound (at least the oracle's
Louvain's modularity less 0.02) with the modularity they return checked
against the oracle's of their partition. `check` fails if any finding is
neither a match nor a documented difference (clustering at self-loops).

## The head-to-head (`head-to-head`)

pg_warren timed against pgGraph, python-igraph and Neo4j, with plain Cypher
and with the Graph Data Science library, on the same machine and the same
tables
(`head_to_head/`; results and caveats in
`bench/results/corpus/head_to_head/README.md`). Each engine is an adapter
behind one port (`head_to_head/engine.py`): it loads the dataset (its
phases timed: pg_warren's COPY and `refresh`; pgGraph's COPY, the primary
key it needs on `edges`, its registration and `graph.build()`; igraph's CSV
read and graph build; `neo4j-admin import`, the server start, the id index and GDS's
projections), then `timing.py` runs each workload of `workloads.py` cold
once and warm up to `--repeat` times (5; fewer once `--budget`, 120 s, is
spent), each run bounded by `--timeout`. A time counts only when the
engine's answer agrees with the reference (`answers.py`: the igraph oracle
under pg_warren's definitions; PageRank under the convention each engine
computes, `pagerank.py`); otherwise the report shows why not.

| Engine | Cold run | Threads |
|---|---|---|
| `pg_warren` | after `warren.evict`: the version loaded from its segments | `warren.threads` (`--threads`, 4) |
| `pggraph` | after `graph.unload_graph`: the graph loaded from its persisted artifact | 1 (a query runs on its backend's thread) |
| `igraph` | the first call (igraph keeps nothing between calls) | 1 (igraph's algorithms are sequential) |
| `neo4j-cypher` | after a server restart (empty page cache, plan cache and JIT) | 1 (Community runs a query on one thread) |
| `neo4j-gds` | after a server restart and new projections | GDS `concurrency` (`--threads`, 4; Community's maximum) |

The workloads: distinct out-degree; BFS reach within 6 hops and BFS depths
from the start node (the most out-edges, or the hub when undirected);
hops (or Dijkstra cost when weighted) between the correctness suite's 25
seeded pairs; 20 PageRank propagation steps; weak components; triangles;
Louvain and Leiden (held to the correctness suite's quality bound);
exact betweenness where N·M ≤ 5·10⁸. Cypher runs what one query can
express, and pgGraph what its SQL API offers (reach, depths, paths and
components); each records why it cannot run the rest.

Neo4j Community 2026.10.1 and GDS 2026.10.1 (the pair GDS's
`versions.json` names) are downloaded into `<cache>/head-to-head` and
checked against `head_to_head/pins.json` (Neo4j's published sha256; GDS
publishes none, so its pin is the first fetch's). Java 21 must be on
`PATH`. The server runs in the foreground with its state under the cache,
heap and page cache from `--neo4j-heap` (4g) and `--neo4j-pagecache` (2g),
auth off and Bolt on 127.0.0.1:7687; each dataset's database is deleted
once measured. The runner's logic is tested against a fake engine, and
the igraph engine end to end, in `bench/tests/test_corpus_head_to_head.py`;
Neo4j itself never runs in CI, and neither does pgGraph.

pgGraph 1.2.1 is built from source, as `head_to_head/pins.json` records
(its tag and commit, Rust 1.96.0 and cargo-pgrx 0.19.1, release profile),
and installed into the same private PostgreSQL tree as pg_warren. Its
cargo-pgrx goes into a separate `CARGO_INSTALL_ROOT` with its own
`PGRX_HOME`, so the repository's cargo-pgrx 0.19.3 is untouched:

```sh
H=/var/tmp/warren-corpus/head-to-head
git clone --branch v1.2.1 https://github.com/Evokoa/pgGraph $H/pggraph-src
rustup toolchain install 1.96.0 --profile minimal
CARGO_INSTALL_ROOT=$H/cargo-pgrx-0.19.1 cargo +1.96.0 install cargo-pgrx --version 0.19.1 --locked
mkdir -p $H/pgrx-home
printf '[configs]\npg16 = "/usr/lib/postgresql/16/bin/pg_config"\n' > $H/pgrx-home/config.toml
(cd $H/pggraph-src/graph && PATH=$H/cargo-pgrx-0.19.1/bin:$PATH PGRX_HOME=$H/pgrx-home \
    cargo pgrx package --no-default-features --features pg16 \
    --pg-config /usr/lib/postgresql/16/bin/pg_config --out-dir $H/pggraph-pkg)
cp -r $H/pggraph-pkg/usr/* $T/usr/   # $T: the private tree pg_warren is installed in
```

When `pggraph` runs, the cluster also gets pgGraph's recommended settings
(`pggraph_engine.CLUSTER_SETTINGS`: `shared_preload_libraries = 'graph'`
and a 4 GB `graph.memory_limit_mb`, with manual sync).

## The nightly job (`.github/workflows/corpus.yml`)

The tiny tier runs every night (02:23 UTC, with `ci.yml`'s nightly) and on
demand (`workflow_dispatch`), never on a pull request or a push: it reads
third-party hosts (SNAP, Newman's netdata, KONECT, Netzschleuder, the KGE
mirror; several others were unreachable while the suite was built), so it
must not gate merges. It installs a release build into the runner's PG 16,
Node 22 and the pinned keepset (`npm ci` from the committed lock, cached on
the lock's hash), restores the tiny tier's tables from a cache keyed on
`pins.json`, the manifest and the code that converts bytes into tables (a
miss fetches, and a fetch whose bytes differ from the pins fails), fails if
the fetch pinned anything new (`pins.json` must not change), then runs
`suite.py check --tier tiny` against NetworkX and igraph. A finding that is
neither a match nor a documented difference fails the run red; the step
summary has the correctness table (`suite.py summary`) and the results are
uploaded as the `corpus-tiny` artifact. It is bounded at 60 minutes. Locally
(4 shared vCPUs, release build) the fetch takes 25 s and the check 3 min,
2.5 of them WN18RR's betweenness (pg_warren and igraph, about 3.8·10⁹ edge
visits each).

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
