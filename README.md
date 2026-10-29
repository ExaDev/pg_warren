# pg_warren

Deep traversal and whole-graph computation for PostgreSQL, over graphs whose
nodes and edges live in your ordinary tables.

pg_warren builds a compact, memory-resident CSR adjacency structure from two
SQL queries (one for nodes, one for edges) and exposes it through `warren.*`
functions: BFS, reachability, shortest paths, typed path walks, semiring
SpMV, PageRank, label propagation, a Lanczos eigensolver, and distances
between graph versions.

It is not a graph database and has no query language. Your tables stay the
source of truth, and `warren.*` calls compose with ordinary SQL on either
side: node ids go in, rows keyed by node id come out, and you join them
back. Think "pgvector for graphs": an index plus compute engine beside your
data, not a replacement for it.

Every build is an immutable **version** with stable dense ids, stored
transactionally in extension tables. A version can be built inside the
transaction that changes the data, compared with the current one, and
committed or rolled back with it.

## A ten-line example

After [installing](#install) it and running `CREATE EXTENSION pg_warren;`:

```sql
CREATE TABLE people  (id bigint PRIMARY KEY, name text NOT NULL);
CREATE TABLE follows (src bigint NOT NULL, dst bigint NOT NULL);
INSERT INTO people  VALUES (1, 'ada'), (2, 'bob'), (3, 'cy'), (4, 'dee'), (5, 'eve');
INSERT INTO follows VALUES (1, 2), (2, 3), (3, 1), (3, 4), (4, 5);

SELECT warren.create_graph('social', 'SELECT id FROM people', 'SELECT src, dst FROM follows');
SELECT warren.refresh('social');                                   -- builds version 1

SELECT p.name, b.depth                                             -- who ada reaches in 3 hops
FROM warren.bfs('social', ARRAY[1::bigint], 3) b
JOIN people p ON p.id = b.node_id ORDER BY b.depth, p.name;
--  ada 0 | bob 1 | cy 2 | dee 3

SELECT warren.pagerank('social', 'pr');                            -- writes vector 'pr'
SELECT p.name, round(r.value::numeric, 3) AS rank
FROM warren.vector_topk('social', 'pr', 3, NULL::bigint) r
JOIN people p ON p.id = r.node_id ORDER BY rank DESC, p.name;
--  cy 0.243 | bob 0.209 | eve 0.209
```

The graph's ids are `bigint`, so id arguments are cast (`1::bigint`), and
functions with no id argument take a type witness (`NULL::bigint`) for their
result type. A traversal reads the version, not the tables: rows inserted
after `refresh` appear after the next `refresh`
([docs/semantics.md](docs/semantics.md)).

## How it compares

pg_warren was timed against [igraph](https://igraph.org/) (python-igraph 1.0.0)
and Neo4j 2026.10 Community. Neo4j ran both as plain Cypher and with the
Graph Data Science library (GDS). All three ran on the same machine, over 12
real graphs from the benchmark corpus, from 77 nodes up to 1.4M nodes and
3.8M edges. A time counts only if that engine's answer matched the reference.

### Features

| | pg_warren | igraph | Neo4j (Cypher) | Neo4j + GDS |
|---|:---:|:---:|:---:|:---:|
| **Where it lives** | | | | |
| Runs inside PostgreSQL, over your existing tables | ✅ | ❌ | ❌ | ❌ |
| No separate service to run | ✅ | ✅ (in-process library) | ❌ | ❌ |
| Queried from SQL, joined with relational data | ✅ | ❌ | ❌ | ❌ |
| **Consistency and freshness** | | | | |
| Analytics see the live data with no rebuild | ❌ (reads a built version) | ❌ (load on each run) | ✅ | ❌ (projections) |
| Graph built inside your transaction, rolled back with it | ✅ | ❌ | ✅ (the store is the graph) | ❌ |
| Versioned graphs: stage, diff, promote | ✅ | ❌ | ❌ | ❌ |
| Replicated to streaming standbys | ✅ | ❌ | Enterprise | Enterprise |
| **Querying and algorithms** | | | | |
| Graph query language, pattern matching | ❌ (SQL functions) | ❌ | ✅ | ✅ |
| Traversals (BFS, reach, k-hop, paths) | ✅ | ✅ | ✅ | ✅ |
| Centralities, components, community detection | ✅ (core set) | ✅ (very broad) | ❌ | ✅ (broad) |
| Node embeddings, link prediction, ML pipelines | ❌ (building blocks only) | ❌ | ❌ | ✅ |
| Semiring linear algebra, spectral analysis | ✅ | partial | ❌ | ❌ |
| Multi-threaded | ✅ | ❌ | Enterprise | ✅ (4 threads in Community) |
| **Maturity** | | | | |
| Production track record, tooling, visualisation | ❌ (pre-1.0) | ✅ | ✅ | ✅ |

### Performance

Times are warm medians.

| Workload | pg_warren | igraph | Neo4j + GDS | Verdict |
|---|---:|---:|---:|---|
| Shortest paths, 25 pairs (dblp) | **6 ms** | 1.04 s | 2.22 s | 🟢 Fastest on 11 of 12 graphs |
| Louvain communities (dblp) | **1.16 s** | 5.49 s | 12.7 s | 🟢 Faster than GDS on every graph, than igraph on 7 of 12 |
| Triangles (hetionet) | **0.12 s** | 0.49 s | 2.88 s | 🟢 Faster than igraph on 10 of 12 graphs |
| Time to first query | **0.01–3.5 s** | about the same | 13–18 s | 🟢 No server to start or data to import |
| PageRank, 20 iterations (roadNet-TX) | **0.22 s** | 0.81 s | 0.62 s | 🟢 Faster than igraph on 7 of 12 graphs; slower on 5, by 1.2–3× and on the tiny lesmis 100× (a call's fixed cost); faster than GDS on all 12 |
| BFS and connected components | 🟡 | **fastest** | slower | 🟡 Ahead of GDS; igraph 1.2–60× faster |
| Leiden communities (FB15k-237) | 0.21 s | **0.14 s** | 0.46 s | 🟡 Within 1.6× of igraph on medium graphs, faster on 4 of 12; faster than GDS on 7 of 11 |

Plain Cypher is not in the table. It cannot express PageRank, components,
Louvain, Leiden or betweenness. On the larger graphs, its traversal and
triangle queries ran past a 300-second timeout.

**Caveats.** Most rows are one run on a shared 4-vCPU VM. pg_warren and GDS used
4 threads; igraph and Cypher are single-threaded. Every time includes the
client round trip. pg_warren pays a fixed 0.2–2 ms per call, which dominates
on tiny graphs. The Louvain and loading figures predate two later
optimisations, so pg_warren's times there may now be lower. The triangles
row was re-timed after them, and the PageRank and Leiden rows on 0.8.0's
native `pagerank` and `leiden`, against igraph only (PageRank in three
interleaved rounds on all 12 graphs). GDS was not re-run: every GDS figure
is from the earlier run, against 0.7.0. Full tables, settings and caveats are in
[bench/results/corpus/head_to_head](bench/results/corpus/head_to_head/README.md).

## The principle: meet Postgres users where they are

pg_warren is an escape valve for graph workloads that Postgres alone handles
badly, without leaving Postgres or fighting it. Your data stays in your
tables. Node programs are SQL: PL/pgSQL loops over whole-graph calls, with
per-node logic as SQL expressions. Transactions, backups and replicas cover
the graph like everything else, because its versions are ordinary rows.

Pregel-style computation (`gather`, the algorithm library, PL/pgSQL loops
over `spmv` and `vector_map`) is the first-class way in. Sparse matrix
algebra beyond `spmv`/`spmm` (SpGEMM, sparse vectors, and CPU and later GPU
backends) is **planned** as the power-user path; it is not there yet.

## Install

pg_warren supports PostgreSQL 15, 16, 17 and 18 (16 is the development
baseline). `CREATE EXTENSION pg_warren` does not need a superuser (below).
Packages are published from 0.7.0 on; for an earlier release, build from
source.

**Debian 12 and 13, Ubuntu 22.04 and 24.04 (amd64, arm64)**: a signed apt
repository of `postgresql-NN-pg-warren` packages, built against
[PGDG](https://wiki.postgresql.org/wiki/Apt)'s PostgreSQL, whose
`postgresql-NN` they depend on (add PGDG first unless your distribution
ships that major). Add the key and the source once:

```sh
sudo install -d /etc/apt/keyrings
curl -fsSL https://exadev.github.io/pg_warren/apt/pg_warren.asc \
  | sudo tee /etc/apt/keyrings/pg_warren.asc >/dev/null
echo "deb [signed-by=/etc/apt/keyrings/pg_warren.asc] https://exadev.github.io/pg_warren/apt $(. /etc/os-release && echo "$VERSION_CODENAME") main" \
  | sudo tee /etc/apt/sources.list.d/pg_warren.list
sudo apt update
sudo apt install postgresql-17-pg-warren   # or -15, -16, -18
```

Check the key before trusting it: `gpg --show-keys /etc/apt/keyrings/pg_warren.asc`
must show the fingerprint
`A7FF D766 EE91 5EE8 E970  85F8 DF33 D764 5F72 1A34` <!-- apt key -->,
which every GitHub Release's notes repeat.

The package does not edit `postgresql.conf`. Add the preload yourself
(below: on PG 15 and 16 it is what lets backends share loaded graphs) and
restart: `shared_preload_libraries = 'pg_warren'` in
`/etc/postgresql/17/main/postgresql.conf`, then
`sudo systemctl restart postgresql@17-main`.

**Docker**, the quickest way to try it: PGDG's official `postgres` image
(Debian 13) with the package installed and pg_warren preloaded, for amd64
and arm64:

```sh
docker run --rm --shm-size=1g -e POSTGRES_PASSWORD=postgres -p 5432:5432 ghcr.io/exadev/pg_warren:pg17
```

Tags are `pgNN` (the latest release) and `X.Y.Z-pgNN`. `--shm-size` sizes
the container's `/dev/shm`, where the shared cache keeps loaded graphs
(below); Docker's default is 64 MB. `1g` shares graphs of up to about
750 MB; for the default cap of 4GB, give it `6g`. `docker/Dockerfile`
builds an image from source instead
(`docker build -f docker/Dockerfile --build-arg PG_MAJOR=17 -t pg_warren:pg17 .`).

**macOS (Homebrew)**, for Homebrew's `postgresql@17` and `postgresql@18`; a
bottle on Apple silicon, built from source (with Rust) on Intel:

```sh
brew install exadev/tap/pg_warren
```

Then add `shared_preload_libraries = 'pg_warren'` to
`$(brew --prefix)/var/postgresql@18/postgresql.conf` and
`brew services restart postgresql@18` (`brew info pg_warren` repeats this).
macOS is not yet a supported platform: CI runs the tests there without
gating on them, and the shared cache, which measures `/dev/shm` (absent on
macOS), is expected to keep a copy per backend (DESIGN.md §13, M11).
Postgres.app is not supported for now.

**Other Linux (glibc 2.35 or later)**: each GitHub Release has
`pg_warren-X.Y.Z-pgNN-linux-{x86_64,aarch64}.tar.gz`, built on Ubuntu 22.04.
Copy its files where that PostgreSQL's `pg_config` says (its `INSTALL`
says the same), then preload and restart as above:

```sh
cp pkglibdir/* "$(pg_config --pkglibdir)"
cp sharedir/extension/* "$(pg_config --sharedir)/extension"
```

RPM packages (RHEL, Fedora) and PGXN are planned, not built yet.

**Managed PostgreSQL** (Amazon RDS and Aurora, Cloud SQL, Azure, Supabase,
Neon and the like) installs only the extensions the provider ships, so
pg_warren cannot be used there until a provider adopts it.

**From source**, with Rust (the toolchain pinned in `rust-toolchain.toml`),
the server headers (`postgresql-server-dev-NN`), `clang`/`libclang-dev`, and
cargo-pgrx at exactly the version the extension uses:

```sh
git clone https://github.com/ExaDev/pg_warren.git
cd pg_warren
git checkout v0.7.0   # the latest release; main is the version in development
cargo install cargo-pgrx --version 0.19.3 --locked
cargo pgrx init --pg16 "$(command -v pg_config)"
cd crates/pg_warren
cargo pgrx install --release --pg-config "$(command -v pg_config)"
```

Then, in each database:

```sql
CREATE EXTENSION pg_warren;
SELECT warren.version();
```

pg_warren is a *trusted* extension, so this does not need a superuser: any
role with `CREATE` on the database can run it, as on hosted Postgres. That
role becomes the extension's owner. It may update the extension
(`ALTER EXTENSION pg_warren UPDATE`) or drop it, which drops every graph in
the database. It gets no direct access to the `warren.*` tables, which belong
to the bootstrap superuser. Every role, the installer included, reaches
graphs only through the `warren.*` functions and their access checks
(DESIGN.md §7).

On PG 15+ the database owner also owns schema `public`. A graph's source
queries run as the graph's owner, under the `search_path` it was created
with. If that path includes a schema another role can create in, such as
`public` when someone else owns the database, that role could add a function
or operator that the queries then call as the graph's owner at every
`refresh`. Schema-qualify the functions and operators in source queries, or
create graphs under a `search_path` of schemas only their owner can write;
`create_graph` raises a `NOTICE` when it finds such a schema.

Graphs, versions and vectors are ordinary tables, so a superuser's
`pg_dump` includes them, and a plain (`psql`) or custom (`pg_restore`)
restore brings them back. The roles that own and read graphs must exist
first. `pg_dump` warns about circular foreign-key constraints on
`warren.versions` (it references itself) and suggests `--disable-triggers`:
ignore that hint, which would skip pg_warren's catalog checks, and restore
normally. Because only that superuser can read the catalog, a non-superuser's
`pg_dump` of the database fails while pg_warren is installed. Dump as a
superuser, or leave the graphs out with `--exclude-table-data='warren.*'`
and, after the restore, create them again with `warren.create_graph` and
`warren.refresh`.

**The shared cache.** A loaded graph version is kept once per cluster, in
dynamic shared memory, and mapped by every backend that reads it, so pooled
connections share one copy instead of loading one each. It is on by default
(`warren.shared_cache_memory = 4GB`, cluster-wide). On PostgreSQL 17 and 18
it needs nothing configured. On 15 and 16 it needs pg_warren preloaded
(a restart):

```
shared_preload_libraries = 'pg_warren'
```

Without that, or where `/dev/shm` is too small, each backend loads its own
copy, as before 0.5.0, rather than fail. Sizing, containers and the other
settings are in [docs/limits.md](docs/limits.md#the-shared-cache).

**Upgrading** from an earlier release: install the new build, then run
`ALTER EXTENSION pg_warren UPDATE;` in every database that has the extension,
*before* calling any `warren.*` function. The new library is loaded at once,
but the catalog changes only with the update, so calls in between can fail.

## When to use it: frontier size, not hop count

A graph query has two forms, and you choose per query:

- **Shallow, small frontier:** a recursive CTE or self-joins over your
  `nodes`/`edges` tables. Fully transactional, planner-optimised, never
  stale. This is the default and covers most queries.
- **Deep, large frontier:** one `warren.*` call, joined back to your tables
  for filtering and projection. Reads a version, not the live tables.

The criterion is **how many rows the traversal touches, not how many hops it
takes.** Three hops at average degree 10 is about a thousand intermediate rows
and belongs in SQL; three hops at degree 1,000 is a billion and belongs in
the engine. Rule of thumb: if `EXPLAIN` of the CTE shows the recursive term
producing more than about 10⁵–10⁶ rows, or the computation is iterative at
all (PageRank, components, propagation), move it into pg_warren. Both forms
produce rows keyed by node id, so switching changes one clause, not the
surrounding query.

## The SQL surface at a glance

Every function that works on a graph takes its name first and, where it
reads the graph, an optional `version` (default: the current version).
`version`, `last_stats`, `s`, `steps` and `shared_cache` take no graph.

| Area | Functions |
|---|---|
| Graphs and versions | `create_graph`, `refresh`, `promote`, `drop_version`, `compact`, `diff`, `info`, `drop_graph`, `grant`, `revoke`, `prewarm`, `evict` |
| Traversal | `bfs`, `neighbours`, `degree`, `has_edge`, `reachable`, `reachable_set`, `shortest_path`, `dijkstra`, `components`, `walk`, `last_stats` |
| Walk steps | `s` and `steps` (jsonb), which build the composite type `warren.step`; the cast `jsonb::warren.step[]` |
| Vectors | `vector_from_query`, `vector_const`, `vector_degree`, `vector_dense_id`, `vector_values`, `matrix_rows`, `vector_topk`, `vector_stats`, `id_map`, `drop_vector` |
| Compute | `spmv`, `spmm`, `vector_map`, `explain_map`, `vector_reduce`, `vector_norm_diff`, `gather` |
| Library | `pagerank`, `eigenvector_centrality`, `katz_centrality`, `hits`, `components_vector`, `label_propagation`, `label_propagation_majority`, `sssp_vector`, `degree_vector`, `aggregate_neighbours`, `triangles`, `clustering`, `core_number`, `strong_components_vector`, `topological_order`, `dag_longest_path`, `minimum_spanning_forest` |
| Spectral | `eigen`, `distance` |
| Extension | `version`, `shared_cache` |

Signatures are in `\df warren.*` and in DESIGN.md (§6, §8, §9.3).

## Documentation

- [docs/semantics.md](docs/semantics.md): graphs, versions, dense ids,
  staleness, transactions, retention, access, id types.
- [docs/traversal.md](docs/traversal.md): `bfs`, paths, components, `walk`
  and its steps, `last_stats`.
- [docs/compute.md](docs/compute.md): vectors, semiring products, masks,
  `vector_map`, `gather`, the algorithm library, `eigen` and `distance`.
- [docs/communities.md](docs/communities.md): contraction to community-level
  graphs, `project` and `lift`; Louvain, Leiden and modularity.
- [docs/change-detection.md](docs/change-detection.md): measuring structural
  change before you commit it, in one transaction or with a shadow graph.
- [docs/limits.md](docs/limits.md): settings, memory, measured performance,
  known limitations.
- [DESIGN.md](DESIGN.md): the full design and the reasons for it.

Every SQL example in the README and `docs/` runs in CI, in
`test/sql/docs_examples.sql`.

## Status

- **0.1.0**: versioned graphs and traversal, milestones M0–M3 of
  DESIGN.md §13.
- **0.2.0**: vectors, semiring compute, `gather`, the algorithm library,
  `eigen` and `distance`, milestones M4–M6.
- **0.3.0**: hardening, milestone M7. Memory is bounded by default (the
  cache, the whole build, and each call, with 53400 refusals before work
  that cannot fit), pg_warren installs without a superuser, dumps restore,
  and a graph of several relations may leave its merged CSR unstored
  (`store_merged := false`). Upgrading needs `ALTER EXTENSION pg_warren
  UPDATE`, and changes some behaviour (see the changelog).

  A graph of 10M nodes and 100M edges builds and runs within the default
  limits (each 4 GB) with one relation, or with five weighted relations and
  `store_merged := false` (DESIGN.md §12; measured on 4 vCPUs). A backend's
  peak is bounded by the cache and compute limits together (and the build
  limit while a `refresh` or `contract` runs), not by 4 GB:
  on the five-relation graph `pagerank` peaks at 4.5 GB and `diff` at
  7.2 GB. The caveats: the five-relation graph beside `pagerank`'s scratch
  is 30 MB over the cache cap (the excess is charged to compute memory) and
  its all-relations reads are slower; with its merged CSR it is 6.0 GB, so
  each statement reloads it, and `diff`, `distance` and `eigen` are refused
  unless the limits are raised. Several §12 targets are missed on 4 vCPUs:
  BFS through SQL is 11× a recursive CTE rather than 20×, `reachable_set`
  26M rather than 50M edges/s, `pagerank` on five relations 36–40 s rather
  than 30 s, its `diff` 58 s rather than 30 s, and `components` without the
  merged CSR is borderline at 10.1 s; `eigen` does not converge within
  10 minutes on a uniform random graph of that size
  ([docs/limits.md](docs/limits.md)).

- **0.4.0**: locality, the library and communities, milestone M8.
  Dense ids can be ordered for locality
  (`create_graph(…, node_order := 'locality')`, `compact(…, node_order)`;
  DESIGN.md §14 item 22). The library gains eigenvector and Katz
  centrality, HITS, clustering and majority-vote label propagation; k-cores,
  strong components, topological order and longest paths on DAGs, and
  minimum spanning forests; betweenness, closeness and harmonic centrality,
  exact or from sampled sources; and neighbourhood similarity for link
  prediction (DESIGN.md §6.7). Contraction builds community-level graphs
  (`contract`, with `project` and `lift` between the levels, DESIGN.md
  §6.6), and Louvain and Leiden community detection are written over it
  (`louvain`, `leiden`, `modularity`;
  [docs/communities.md](docs/communities.md)). Upgrading needs
  `ALTER EXTENSION pg_warren UPDATE` (see the changelog).

  The 100M-edge soak, re-run, moved no §12 row beyond run-to-run spread.
  At 10M nodes `louvain` and `leiden` find the communities of a block
  graph in 115 s and 247 s, but on a graph with little community structure
  `leiden` is refused (53400) at the default `warren.max_compute_memory`
  and `louvain` writes level graphs faster than they shrink (the soak
  stopped it for disk after 7.5 GB); a statement that runs `contract`, as both do, is bounded by
  the cache, compute and build limits together
  ([docs/limits.md](docs/limits.md)).

- **0.5.0**: the shared cache, milestone M9. A loaded version, and a
  persisted vector, is kept once per cluster in dynamic shared memory and
  mapped by every backend that reads it (`warren.shared_cache_memory`,
  default `4GB`; `warren.shared_cache()` shows what it holds; DESIGN.md
  §5.6). It needs no configuration on PostgreSQL 17 and 18, and
  `shared_preload_libraries = 'pg_warren'` on 15 and 16, which otherwise
  keep 0.4.0's copy per backend. The graph reads that use no vector are
  `PARALLEL SAFE`. `gather` gains the `mode` combiner and `include_self`.
  Upgrading needs `ALTER EXTENSION pg_warren UPDATE` (see the changelog).

  Behind PgBouncer, 50 pooled connections on one graph hold 338 MB instead
  of 6.63 GB, and a pool's cold start is one load; a per-row read over 1M
  rows runs 2.6–3.6× faster with 2–4 parallel workers. In the 100M-edge
  soak a version larger than `warren.max_cache_memory` is no longer
  reloaded by every statement. The caveats: shared versions live in
  `/dev/shm`, so a Docker container needs `--shm-size` (else versions are
  loaded privately); wherever a version is read privately, each parallel
  worker loads its own copy, and a short query forced parallel can be
  slower; and Louvain and Leiden at 10M nodes still write several GB of
  level graphs ([docs/limits.md](docs/limits.md)).

- **0.6.0**: identity, lifecycle and visibility, milestone M10. Nodes
  can be keyed on strings: `text` is a fourth id type, for IRIs and slugs,
  compared bytewise (docs/semantics.md, "Text ids"). A graph's ownership
  can be transferred (`warren.set_owner`); a dropped owner role's graphs,
  vectors and reader entries are handled by `warren.reassign_owned` and
  `warren.drop_owned`, an optional superuser's `DROP OWNED` trigger, and
  `warren.orphans()` and `warren.drop_orphans()`; a graph's owner can
  bound the vectors other roles persist on it
  (`warren.set_vector_quota`). `warren.cache()`, `warren.backends()` and
  `warren.progress()` show what each backend's cache holds and counts and
  how far each long call has got (DESIGN.md §5.7). Upgrading needs `ALTER
  EXTENSION pg_warren UPDATE` (see the changelog).

  In the 100M-edge soak with IRIs as node ids, the version is 28% larger
  than with `bigint` ids and the build takes 1.39× as long; `pagerank`,
  `eigen` and the community detections are within 12%, the library within
  1.0–1.4×, and calls that return a row per node (`bfs`, `components`)
  take about twice as long, since they build each id's `text`. The build maps edge endpoints on the compute pool for every id
  type: the `bigint` graph builds in 79 s, from 173 s in 0.5.0's soak. The
  caveats: `walk` and `neighbours` still build their rows before
  returning the first; and on PostgreSQL 15 and 16, other backends' rows
  in `warren.backends()` and `warren.progress()` need
  `shared_preload_libraries` ([docs/limits.md](docs/limits.md)).

- **0.7.0**: proven on real graphs and real deployments, milestone M11.
  A suite of 19 real graphs pinned through
  [keepset](https://mearman.github.io/keepset/) (`bench/corpus`) checks
  results against NetworkX and igraph, and times pg_warren against igraph
  and Neo4j (How it compares, above); Appendix A's use cases ran at 10M
  nodes. A pair of nodes linked in several relations is read one way
  everywhere: strengths are summed and an unweighted pair counts once,
  costs take the pair's best edge, and `per_relation := true` reads it as
  an edge in each relation (docs/semantics.md, "A pair in several
  relations"). A relation with few edges stores sparse offsets
  (FB15k-237's version is 7.5× smaller), and `eigen` takes each connected
  component's eigenpairs exactly. Concurrent refreshes, crash recovery,
  streaming replicas (where writes are refused with 25006), SQL fuzzing
  and a connection pooler were tested, and what they found is fixed.
  Packages are published: a signed apt repository, Docker images, a
  Homebrew tap and tarballs (Install, above). Upgrading needs `ALTER
  EXTENSION pg_warren UPDATE` and a `refresh` of every graph, and changes
  some results on graphs whose pairs are linked in several relations (see
  the changelog).

  Appendix A at 10M nodes, on generated data, passes 65 of 70 checks.
  Among the misses, A.3's first `gather` superstep takes 29 s with `text` messages
  against its 20 s target, cross-graph `distance('edit')` is refused
  (53400) by an over-estimate of its memory, and `eigen` on A.2's dense
  bridge takes 1,332 s against 10 minutes; A.1 and A.3 ran at 7M and
  11.5M edges rather than 100M and 50M, for the disk. Head to head,
  pg_warren loses on PageRank, Leiden and a fixed cost of 0.2–2 ms a call.
  The caveats: the corpus's large tier and a multi-day soak behind a
  pooler have not run (M12); macOS is not a supported platform; and RPM
  packages and PGXN are not built yet
  ([docs/limits.md](docs/limits.md)).

- **0.8.0** (in development, M12): toward 1.0.0: the review of the whole
  SQL surface before it is frozen; the runs moved from M11 (the corpus's
  large tier, a multi-day pooler soak, Appendix A.1 and A.3 at their full
  edge counts); and the performance leads M11 found, among them Leiden
  over the previous level's graph and PageRank as a kernel (DESIGN.md
  §13).

See [CHANGELOG.md](CHANGELOG.md). This is pre-1.0 software: the SQL surface
may still change between minor versions, with an upgrade script each time.

Not there yet: the rest of the road to 1.0 (DESIGN.md §13) is a
reviewed, frozen SQL surface, with the corpus's large tier, a multi-day
soak behind a pooler and Appendix A.1 and A.3 at their full edge counts
run before its release candidates (M12, 1.0.0). Incremental
maintenance, general sparse matrix algebra (contraction's `PᵀAP` is the
one sparse product) and a GPU backend are left to 1.x
([docs/limits.md](docs/limits.md)).

## Positioning vs prior art

pg_warren is a graph index and compute engine for PostgreSQL. It derives a
compressed-sparse-row adjacency structure from your ordinary tables and runs
deep traversals and whole-graph linear algebra over it from SQL. It is not the
first extension to put a CSR beside Postgres tables.
[pgGraph](https://github.com/evokoa/pggraph) pioneered derived, rebuildable CSR
traversal behind SQL functions, and
[OneSparse](https://github.com/OneSparse/OneSparse) brought the GraphBLAS
semiring model into Postgres as datatypes.

pg_warren's contribution is different:
- Every build is an immutable version with a stable id space.
- Builds are stored transactionally in ordinary tables, so they are WAL-logged,
  replicated, and rolled back with your transaction.
- A version can be built *inside* the transaction that changes the data and
  measured against the current one, structurally or spectrally, before you
  commit.
- One CSR serves both traversal and semiring compute (with GraphBLAS operation
  names).
- Control flow stays in PL/pgSQL, with per-node logic written as SQL
  expressions.
- Contraction gives community-level computation: a quotient graph per
  partition, a real graph version with its own vectors, and `project` /
  `lift` between the levels. Louvain is a short PL/pgSQL program over
  it; Leiden runs the same steps natively, its levels held in memory.

pg_warren takes no code from either project. Ideas adopted from them are
credited in [NOTICE](NOTICE) and in comments where they are used; the reading
is recorded in [docs/prior-art-notes.md](docs/prior-art-notes.md). Nor does
it bind SuiteSparse:GraphBLAS: its kernels are native, and GraphBLAS is read
for its algorithms and vocabulary, not imported (DESIGN.md §6.10).

## Building and contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

Apache License 2.0 — see [LICENSE](LICENSE).
