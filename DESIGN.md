# pg_warren — Index-Free-Adjacency Traversal and Graph Compute for PostgreSQL

**Technical note and build brief — v0.6, 19 October 2026**

> This document is intended to be dropped into an empty repository as the founding brief for an engineering agent. It describes *what* to build, *why* each design decision was taken, and the order in which to build it. Where a decision is deliberately left open, it is flagged in §14. Read the whole document before writing any code.
>
> Changes from v0.1: stable dense IDs across versions (§5.2), multi-version catalog with staging and retention (§5.1, §7), compute layer — vectors, semiring operations, vertex programs, eigensolver (§6), change-detection workflow (§8), `build_reverse` defaults to true, edge relations promoted into v1, milestones revised.
> Changes from v0.2: the query paradigm and the SQL/warren seam made explicit (§9.1); `warren.walk`, an in-engine typed path-pattern traversal with a stable step IR, added to v1 (§9.2); SQL/PGQ-style sugar recorded as the only language direction worth considering (§10).
> Changes from v0.3: §6 rewritten around "SQL all the way down" — the BSP runner and any program DSL are dropped; control flow lives in PL/pgSQL; three tiers of expressiveness (§6.1); `vector_map` takes SQL expression syntax with a compiled fast path (§6.3); `warren.gather` for tier-2 Pregel with SQL as the vertex language and a `Message` trait in the core (§6.4); contraction / project / lift for community-stateful computation (§6.6). Appendix A adds worked use cases with expected outcomes for test derivation.
> Changes from v0.4: §4A added — prior art as of October 2026, with a mandatory pre-M1 code reading of pgGraph and OneSparse. The policy is to build everything ourselves and to read prior art for the lessons it embodies, not to take it as a dependency.
> Changes from v0.5: the §4A prior-art reading is done (`docs/prior-art-notes.md`). Its outcomes: the §4A table is corrected; catalog dump registration, `PUBLIC` revocation, graph ownership and the access model (§5.1, §7); load-time segment validation (§5.3); one pinned snapshot and no source-table locks during builds, counting-sort CSR construction and a reverse CSR built by transpose (§5.4); a Rust-allocated, `Arc`-owned cache (§5.5); GraphBLAS operation naming (§6.3); error and interrupt discipline (§6.9); the bidirectional-BFS meeting rule, visible truncation and canonical component IDs (§9.3); toolchain, lint and CI rules (§11–§13); evidence recorded under §14 items 2, 4 and 16.

---

## 1. Summary

`pg_warren` is a PostgreSQL extension that provides **index-free adjacency (IFA) traversal and whole-graph computation** over graphs whose nodes and edges live in ordinary Postgres tables.

It is **not** a graph database. It does not replace Postgres storage, does not introduce a query language, and does not try to teach the planner about graphs. It is a *graph index plus compute engine*: a compact, memory-resident adjacency structure derived from relational tables, queried through SQL-callable traversal functions, and usable as a sparse matrix for vertex-centric and spectral computation. Think "pgvector for graphs" rather than "Neo4j inside Postgres".

The name: a *warren* is a dense network of interconnected burrows. Traversing one is a matter of following tunnels, not consulting a map.

## 2. Motivation

Postgres already handles shallow graph queries well. An indexed `edges(src, dst)` table plus a recursive CTE will do 1–3 hop neighbourhood queries at speeds comparable to native graph databases.

Two things it does badly:

**Deep traversal.** BFS to depth 6+, reachability over millions of nodes, shortest paths across a large component. The relational approach does one B-tree lookup per visited node, each costing hundreds of nanoseconds to microseconds and touching scattered pages. A CSR-style adjacency structure does the same traversal as contiguous array reads, close to memory bandwidth — one to two orders of magnitude faster.

**Global computation.** PageRank, connected components, label propagation, community detection, embedding propagation, spectral analysis of the adjacency or Laplacian matrix. These are iterative, touch every vertex many times, and are hopeless in SQL. Today teams export to NetworkX/igraph/GraphBLAS in application code, or stand up a separate graph system, and lose the single-source-of-truth property of keeping everything in Postgres.

The target users are teams building large, evolving knowledge systems on Postgres — relational graph tables, pgvector embeddings, full-text — who need an **escape valve** for deep traversal and whole-graph analytics without leaving the database or weakening its consistency guarantees for everything else.

### Design principles

1. **Derived, not authoritative.** The graph structure is built *from* relational tables. The tables remain the source of truth. We never have to solve concurrent mutation of a pointer-rich structure under MVCC.
2. **Compose with SQL, don't replace it.** Traversal functions take node IDs in and return sets of node IDs out; compute functions take and return per-node vectors. Users join those back to their real tables. Postgres does what it's good at on either side; `pg_warren` does what it's good at in the middle.
3. **One substrate, two uses.** CSR serves both as an adjacency structure for traversal and as a sparse matrix for linear algebra. Everything in the compute layer reduces to sparse-matrix × dense-vector operations, which CSR does natively.
4. **Versions are first-class.** A graph build is an immutable snapshot with a stable node-ID space, so builds can be compared with one another. This is what makes change detection possible.
5. **Honest consistency semantics.** Users opt in to staleness, and the extension makes staleness observable.
6. **SQL all the way down.** No query language and no vertex-program language. Control flow is the user's PL; per-row expressions are SQL expressions; the engine is kernels behind ordinary functions.
7. **Small surface area.** Resist feature creep until the core is fast and boring.

## 3. Scope

### In scope (v1)

- Named graphs defined over user tables (or arbitrary SQL) producing a node set and an edge set, with an optional edge relation/type and an optional `float8` weight.
- Directed graphs; undirected is modelled as directed with both edge directions materialised.
- Full rebuild of a graph as a new **version**; stable dense node IDs across versions; configurable retention; staged (unpromoted) versions.
- Persistent storage of built versions in extension-owned tables (survives restarts, included in backups, replicates to standbys).
- Forward and reverse CSR (reverse on by default).
- In-memory cache loaded on first use per backend.
- Traversal primitives: BFS neighbourhood (k-hop), reachability, unweighted and weighted shortest path, connected components; node and relation filters.
- **Typed path-pattern walks** (`warren.walk`): an ordered list of steps, each with relation, direction, per-hop node filter and hop range, executed entirely in-engine, returning end nodes or full paths. The step list is a stable intermediate representation (§9.2).
- **Compute layer (§6):** per-node dense vectors/matrices; semiring SpMV and a SQL-expression `vector_map` (tier 1); `warren.gather` with SQL-typed messages and `collect` (tier 2); an algorithm library written in PL/pgSQL over the primitives; a Lanczos eigensolver for top-*k* eigenpairs of adjacency and Laplacian; spectral and structural distance between versions. No runner, no program DSL: control flow is the user's PL.
- Multi-threaded compute within a backend over extension-owned memory.

### Out of scope (v1) — but keep the door open

- Incremental/streaming updates to the CSR (§10).
- Shared-memory (DSM) cache and a dedicated compute background worker (§10 — likely v1.1).
- Graph contraction / project / lift for community-stateful computation (§6.6 — v1.1; catalog shape fixed in v1).
- User-defined vertex programs via WASM (§6.8 — v1.2+).
- Node properties beyond vectors the user explicitly materialises.
- Any query language (Cypher, GQL, Gremlin), pattern matching with selectivity-aware planning.
- GPU execution.
- Replacing heap storage via the Table Access Method API. Explicitly rejected: see §4.
- Being the first derived-CSR graph index for Postgres. It is not; see §4A. The aim is to be the first *versioned, compute-capable* one.

## 4. Why not a Table Access Method?

The obvious "proper" way to get IFA inside Postgres is a custom TAM with its own page format where nodes hold inline adjacency lists. Considered and rejected:

- **MVCC breaks physical pointers.** TIDs are not stable across `UPDATE`; every edge pointing at a node would need forwarding or version chains. Solving this is most of the work of building a storage engine.
- **The planner doesn't know what to do with it.** A TAM only changes how tuples are stored; a self-join is still planned as a join. The speed-up only materialises if traversal is driven by custom functions anyway.
- **Infrastructure tax.** WAL, vacuum, TOAST, parallel scan, logical decoding, visibility — all must be supported or deliberately stubbed.
- **It is the wrong shape for global compute.** Spectral and vertex-centric algorithms want a dense, contiguous matrix representation, not tuples on pages. A TAM would still need CSR built beside it.

The derived-structure approach captures the benefit for the target workloads at perhaps a tenth of the engineering cost. If demand emerges for transactionally-consistent graph storage, the TAM route can be revisited with a working traversal and compute library in hand.

## 4A. Prior art, and standing on it

Surveyed October 2026. This section exists because the traversal-index half of this design is no longer novel, and the implementing agent should know what already exists before writing a line of the core. The policy is explicit: **`pg_warren` is built in full, in this repository, with no runtime dependency on any of the projects below.** A composite of three extensions with three persistence models and two allocators would be worse than any of its parts. Prior art is read, not imported: its value is that other people have already made and fixed mistakes we have not made yet, and reading their code surfaces those solutions before we need them. Where a specific idea or technique is adopted, the source is credited in a comment and in `NOTICE`.

| Project | What it is | Overlap with this note | Divergence |
|---|---|---|---|
| **pgGraph** (Evokoa; PGXN; Apache-2.0; Rust/pgrx 0.19; PG 14–18; 1.2.1 released 2026-09-23) | Compiles relationships from ordinary tables into forward and reverse CSR, persisted as `.pggraph` artifacts. Each backend loads a validated copy, which Linux backends can share through a sealed memfd. Source tables remain the truth. Its interface is SQL functions (`graph.traverse`, `graph.shortest_path`, `graph.search`) plus bounded GQL/openCypher profiles (`graph.gql`, `graph.cypher`). It also has trigger-based sync into backend-local overlays, RLS/tenant-scoped results, union-find components, integer-weighted Dijkstra, depth/node/frontier breakers and pagination. | §5 (derived CSR, forward+reverse, per-backend cache) and §9 (functions, not a language) almost exactly. Same toolchain. | Persistence is file artifacts, not transactional tables, so it conflicts with the staging and rollback semantics §7–§8 depend on. Generations are published through MVCC catalog rows that point at files. It has no user-visible versions, no stable dense IDs across rebuilds, no standby support, and refuses builds inside a transaction holding source write locks. Weights are `u32` integers, and node and edge counts are capped at 2³². It has no vectors, linear algebra or spectral work. |
| **OneSparse** (Michel Pelletier; Apache-2.0; C; PG18 only; 1.0.1) | Binds SuiteSparse:GraphBLAS into Postgres as `matrix`/`vector`/`scalar`/`graph` expanded-object types with semiring operators; BFS, SSSP, PageRank, triangle centrality, betweenness, square clustering. Depends on libgraphblas and LAGraph. | §6.3 (semiring SpMV) and parts of §6.7. Has done the SuiteSparse binding work that §6.10 debates. | Matrices are a datatype, not a catalog of named graphs built from tables. They load from SQL (`matrix_query`, `matrix_agg`) with caller-supplied integer indices, from large objects, or from server files. A stored matrix is one varlena (≤1 GB) and is deserialised on every call. There are no versions, stable IDs, staging, `gather` with variable-width messages, or contraction. Kernels cannot be cancelled. It is PG18-only because it relies on PG18's `SupportRequestModifyInPlace` for in-place PL/pgSQL updates, a hard constraint against the PG 15–18 target. |
| **pg_igraph** (ineron; PGXN; MIT; C; PG 14+; 1.1.0, 2026-06-11) | Cypher-like language embedded in SQL; edges stored both directions in hash-partitioned tables with covering indexes; adaptive switch from SPI to in-memory hash maps for large frontiers. Requires pg_ilib. | Multi-hop pattern matching and shortest path. | Table-backed with per-query in-memory structures, and a bespoke language — the opposite of §9.1's paradigm. |
| **Apache AGE** | openCypher on Postgres heap tables and indexes. | None at the storage level. | Query language layer; no IFA, no compute. |
| **pgRouting** | Dijkstra, components, etc. over an edge SQL, building a Boost graph in memory per call. | Algorithm vocabulary; `pgr_*` function conventions. | No persistence across calls; rebuilds the graph on every invocation. |
| **DuckPGQ / Kùzu / PuppyGraph** | SQL/PGQ or Cypher engines outside Postgres (DuckDB extension, embedded DB, external engine over Postgres). | SQL/PGQ syntax as the eventual sugar (§10). | Not in-database. Note: SQL/PGQ was removed from PostgreSQL's 19 release branch, so there is no in-core implementation to target. |

### Mandatory pre-M1 code reading

*Done in M0. The record is [`docs/prior-art-notes.md`](docs/prior-art-notes.md), and its outcomes are folded into this note (v0.6). The brief is kept for reference.* Before M1, the agent reads the pgGraph and OneSparse sources end to end and writes `docs/prior-art-notes.md`: a short, specific record of what each project got right, what it got wrong, and what `pg_warren` will do differently and why. §14 item 16 records that the reading was done. What to look for:

1. **pgGraph, structure and loading.** How the CSR is laid out and aligned; how the per-backend snapshot is mapped and invalidated; how `graph.build()` streams from SPI and sorts (tuplesort? in-memory?) and what it does with dangling edges and duplicates; how it bounds frontier and depth and paginates; where its issue tracker and changelog show bugs — these are the mistakes already made. Compare each against §5.3–§5.5 and §9 and change this note where theirs is better.
2. **pgGraph, persistence.** Understand precisely *why* they chose file artifacts over tables (load speed? WAL volume? TOAST cost?) and whether those reasons threaten §5.1's table-backed design at scale. If the numbers say TOAST loading is too slow, §14 item 2's mmap cache moves forward — but the transactional table store stays, because §8 depends on it.
3. **pgGraph, pgrx usage.** How they handle memory contexts, SRF iteration, interrupt checking, extension upgrades and multi-version CI. Copy the patterns that work; this is the fastest way to avoid pgrx-specific traps.
4. **OneSparse, kernels.** How SuiteSparse's semiring dispatch, masks and accumulators are exposed, and what their performance envelope looks like on PG18. `pg_warren`'s native kernels (§6.3) should match its operation vocabulary where sensible so that users moving between the two find familiar names, and its benchmarks give a target for tier 1.
5. **OneSparse, types.** How a sparse matrix is passed through the executor as a datatype (expanded objects, TOAST, in-place modification). `pg_warren` deliberately does not do this — vectors are named, not passed — but the trade-offs they hit inform §14 item 13 (gather sources) and the vector persistence design.
6. **Positioning.** The README states what `pg_warren` adds over pgGraph and OneSparse — versions with stable IDs, staging and change detection, one CSR serving traversal and compute, SQL-only expressiveness, contraction — and credits ideas taken from either.

### What remains novel

Versioned snapshots with a stable dense-ID space; staged builds inside a transaction with rollback semantics; structural and spectral distance between versions; `gather` with SQL-typed messages over the same CSR used for traversal; contraction/project/lift as first-class operations; and the "SQL all the way down" compute model. None of these has a counterpart in the projects above. The traversal core, by contrast, is well-trodden ground, which is exactly why reading the trodden path first is cheap insurance.

## 5. Graph storage architecture

```
┌──────────────────────────────────────────────────────────────┐
│ User tables     nodes(id, ...)   edges(src, dst, rel?, w?)   │
└───────────────┬──────────────────────────────────────────────┘
                │  warren.create_graph(...)   warren.refresh(name, promote)
                ▼
┌──────────────────────────────────────────────────────────────┐
│ Build pipeline (SPI, one snapshot)                           │
│  1. node_sql → match against prior id_map, append new dense  │
│     IDs, tombstone missing                                   │
│  2. edge_sql → remap to dense IDs, counting-sort by src      │
│  3. forward CSR per relation; reverse CSR by transpose       │
│  4. write segments to warren.segments under a new version    │
│  5. promote version (or leave staged)                        │
└───────────────┬──────────────────────────────────────────────┘
                ▼
┌──────────────────────────────────────────────────────────────┐
│ Persistent store                                             │
│  warren.graphs     catalog: name, definition, current_version│
│  warren.versions   one row per build: stats, lsn, staged     │
│  warren.segments   (version_id, segment, chunk_no, bytea)    │
│  warren.vectors    named per-node vectors bound to a version │
│  (a graph may also be derived by contraction of another)     │
└───────────────┬──────────────────────────────────────────────┘
                ▼  loaded on first use, keyed by version_id
┌──────────────────────────────────────────────────────────────┐
│ Backend-local cache  (CSR + vectors; v1.1: DSM shared)       │
└───────────────┬──────────────────────────────────────────────┘
                ▼
┌─────────────────────────────┬────────────────────────────────┐
│ Traversal functions         │ Compute layer                  │
│ bfs, walk, reachable,       │ vectors, spmv, vector_map,     │
│ shortest_path, dijkstra,    │ gather, eigen, contract/       │
│ components                  │ project/lift, version distance │
└─────────────────────────────┴────────────────────────────────┘
```

### 5.1 Catalog

```sql
CREATE TABLE warren.graphs (
    graph_id         serial PRIMARY KEY,
    name             text UNIQUE NOT NULL,
    owner            regrole NOT NULL,  -- set to current_user by create_graph; never user-writable (§7)
    readers          regrole[] NOT NULL DEFAULT '{}',  -- roles (and their members) allowed to query (§7)
    source           text NOT NULL DEFAULT 'query',   -- 'query' | 'contraction' (§6.6)
    node_query       text,              -- SELECT <id> ...                       (source = 'query')
    edge_query       text,              -- SELECT <src>, <dst> [, <rel>] [, <weight>] ...
    search_path      text NOT NULL,     -- the creator's search_path; the source queries always run under it (§5.4)
    id_type          regtype NOT NULL,  -- int4 | int8 | uuid  (text: v1.1)
    directed         boolean NOT NULL DEFAULT true,
    build_reverse    boolean NOT NULL DEFAULT true,
    weighted         boolean NOT NULL DEFAULT false,
    has_relations    boolean NOT NULL DEFAULT false,
    retain_versions  int NOT NULL DEFAULT 2 CHECK (retain_versions >= 1),  -- promoted versions kept
    current_version  bigint                        -- FK to versions (added by ALTER TABLE); NULL until first promote
);

CREATE TABLE warren.versions (
    version_id   bigserial PRIMARY KEY,
    graph_id     int NOT NULL REFERENCES warren.graphs ON DELETE CASCADE,
    seq          bigint NOT NULL,          -- monotonic per graph
    staged       boolean NOT NULL DEFAULT true,
    built_at     timestamptz NOT NULL,
    built_lsn    pg_lsn NOT NULL,
    node_count   bigint NOT NULL,          -- live nodes
    id_space     bigint NOT NULL,          -- dense ID high-water mark (incl. tombstones)
    edge_count   bigint NOT NULL,          -- stored edges summed over relations, after deduplication; undirected edges count twice
    dropped_edges bigint NOT NULL,         -- edge rows dropped: a NULL relation/weight the graph needs, or a NULL, unknown or tombstoned endpoint
    deduplicated_edges bigint NOT NULL,    -- parallel edges folded by deduplication (§14 item 4)
    target_width  smallint NOT NULL CHECK (target_width IN (4, 8)),  -- 4 (u32 targets) or 8 (u64), §5.3
    compacted_from bigint REFERENCES warren.versions ON DELETE SET NULL,  -- set by warren.compact: new ID space (§5.2)
    bytes        bigint NOT NULL,
    label        text,                     -- optional user label, e.g. 'corpus-2026-10-17'
    parent_version bigint REFERENCES warren.versions,  -- set when derived by warren.contract (§6.6)
    label_vector   bigint REFERENCES warren.vectors,   -- the label vector it was contracted on (FK added by ALTER TABLE)
    UNIQUE (graph_id, seq)
);

CREATE TABLE warren.segments (
    version_id  bigint NOT NULL REFERENCES warren.versions ON DELETE CASCADE,
    segment     text   NOT NULL,   -- 'id_map' | 'tombstones' | 'rel_dict' | 'offsets:<r>' | 'targets:<r>' | 'weights:<r>' | 'rev_offsets:<r>' | ...  (<r>: rel_dict index, or 'merged'; §5.3)
    chunk_no    int    NOT NULL,
    data        bytea  NOT NULL,
    PRIMARY KEY (version_id, segment, chunk_no)
);
ALTER TABLE warren.segments ALTER COLUMN data SET STORAGE EXTERNAL;

CREATE TABLE warren.vectors (
    vector_id   bigserial PRIMARY KEY,
    graph_id    int NOT NULL REFERENCES warren.graphs ON DELETE CASCADE,
    version_id  bigint NOT NULL REFERENCES warren.versions ON DELETE CASCADE,
    name        text NOT NULL,
    dtype       text NOT NULL,     -- 'f32' | 'f64' | 'i64' | 'bool'
    width       int  NOT NULL DEFAULT 1,   -- >1 for matrices (N × width, row-major)
    length      bigint NOT NULL,   -- = version.id_space
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (graph_id, version_id, name)
);
CREATE TABLE warren.vector_chunks (
    vector_id bigint REFERENCES warren.vectors ON DELETE CASCADE,
    chunk_no  int,
    data      bytea NOT NULL,
    PRIMARY KEY (vector_id, chunk_no)
);
```

Chunks are fixed-size (8 MiB, `CHUNK_SIZE` in `pg_warren/src/build/chunk.rs`; 1 KiB in `pg_test` builds so that small test graphs span several chunks), numbered from 0 per segment, so a multi-GB graph is many TOASTed rows. Reading a segment concatenates its chunks in order and rejects a gap. Because all of this is ordinary tables, it is WAL-logged, backed up, replicated to standbys, and — crucially for §8 — **transactional**: a version built inside a transaction that then rolls back simply disappears.

The bootstrap SQL registers every `warren.*` table and sequence with `pg_extension_config_dump`. Without that, `pg_dump` omits extension-member data, and graph definitions, versions and vectors would silently be missing from logical dumps.

The bootstrap SQL also runs `REVOKE ALL … FROM PUBLIC` on every `warren.*` table and sequence. No role except the extension owner can read or write the catalog, segments or vectors directly; all access goes through `warren.*` functions. This is what makes the access check in §7 meaningful: segment and vector bytes are reachable only through functions that perform it. The schema itself grants `USAGE` to `PUBLIC`, so any role can call `warren.*` functions (each checks access itself).

### 5.2 Node identity — stable dense IDs

CSR wants dense `0..N-1` integer IDs. User tables have `bigint`, `uuid`, etc.

**Dense IDs are stable across versions of a graph.** This is the key change from v0.1 and the property that lets vectors computed on version *n* be compared with vectors on version *n+1*.

- The first build assigns dense IDs `0..N-1` in `id_type` sort order.
- Each subsequent build starts from the previous promoted version's `id_map`. Nodes present in both keep their dense ID. New nodes are appended at the high-water mark in sort order. Nodes that have disappeared keep their dense ID but are marked in a `tombstones` bitmap; their adjacency rows are empty. Dense IDs are never reused. A node that reappears after being tombstoned gets its old dense ID back and its tombstone cleared.
- `id_space` (high-water mark) therefore grows monotonically; `node_count` is live nodes. Vectors are allocated at `id_space` length; tombstoned slots hold zero/NaN per dtype.
- `warren.compact(name)` is an explicit operation that renumbers to remove tombstones, producing a new version with a **new ID space** that is incompatible with earlier versions for vector comparison (recorded as `compacted_from` on the version row; `warren.distance` refuses to compare across a compaction boundary). Run it when the tombstone fraction is high and no cross-version comparisons are pending. *Implemented (M2, `warren_core::version::compact`):* live nodes get dense IDs `0..node_count` in `id_type` order (as a first build would), the relation dictionary is unchanged, every CSR is rebuilt from the per-relation forward edges through the normal build (weights and `build_reverse` kept; `deduplicated_edges` is 0, since nothing is folded), and the old→new mapping is returned so bound vectors can be carried across. *SQL (M2):* `warren.compact(name) RETURNS bigint` compacts the current version, records the result as a new version with `compacted_from` set (its `dropped_edges`, `deduplicated_edges` and `built_lsn` carried over from the source, since it holds the same edges as of the same snapshot), promotes it with retention, and returns its id; later refreshes continue in the new ID space. **Lineage (M2):** a version whose `seq` is below that of the graph's latest compacted version (`compacted_from IS NOT NULL`) is in a replaced ID space: `promote` refuses to make it current, and `refresh` refuses to build on it, with `object_not_in_prerequisite_state` naming the compaction. A graph therefore never returns to an ID space a compaction replaced, which vectors and `distance` (aligned by dense ID) rely on. *Implemented (M4, `vector::move_vectors`):* `compact` **moves** every persisted vector bound to the source version into the compacted one (decided in M4 review): each is rewritten under the same name and `created_by` with its rows gathered by the old → new renumbering (`Renumbering::old_dense_ids`), so every live node keeps its value and tombstoned rows are dropped, then deleted from the source; one vector is held at a time, and a `NOTICE` names the vectors moved. Moving rather than copying lets retention clear the old id space: after a compaction the source cannot be promoted, refreshed from or compared across (`diff`, `distance`, `vector_norm_diff` refuse), so vectors left on it would only pin dead storage. Transient vectors (§6.2) are not moved; they stay keyed to the source until freed. Dropping the source version later sets `compacted_from` to NULL; `diff` then still refuses through `warren-core`'s dense-ID check.
- `id_map` is stored as (external ID, dense ID) pairs sorted by external ID so external→dense is a binary search; an inverse array gives dense→external in O(1).
- Traversal and compute functions accept and return **external IDs** by default; vector materialisation to tables goes through `id_map`.

### 5.3 CSR layout

Per relation *r* (a single implicit relation, named `''` in `rel_dict`, if `has_relations = false`). In segment names *r* is the relation's index in `rel_dict`, which is append-only across versions, so a relation keeps its segment names; the merged CSR's segments use `merged` (`offsets:merged`, …) and are stored only when there is not exactly one relation:

| Segment | Type | Length | Notes |
|---|---|---|---|
| `offsets:r` | `u64[]` | id_space+1 | `offsets[i]..offsets[i+1]` indexes `targets` |
| `targets:r` | `u32[]` / `u64[]` | M_r | dense destination IDs, sorted within row |
| `weights:r` | `f64[]` | M_r | if `weighted` |
| `rev_offsets:r`, `rev_targets:r`, `rev_weights:r` | | | if `build_reverse` |
| `id_map` | per `id_type` + `u64` | N | sorted by external ID |
| `tombstones` | bitmap | id_space | |
| `rel_dict` | text[] | R | relation names → index |

`u32` targets when `id_space < 2³²`, else `u64`; width recorded in `versions.target_width`. Sorted targets allow `has_edge` by binary search and make set-intersection algorithms (triangles, Jaccard) straightforward.

Per-relation CSRs are stored separately so a traversal or SpMV can be restricted to a relation mask without filtering; a "merged" all-relations CSR is built additionally if `has_relations` (it is the common case for global computation). An M1 benchmark checks this choice against the alternative of a single merged CSR with a parallel adaptive-width (`u8`/`u16`/`u32`) relation array, which pgGraph uses and which needs about half the storage when `has_relations`. **Decided (M1): keep per-relation CSRs plus the merged CSR.** `benches/relation_layout.rs` (100K nodes, 1M weighted edges, 5 relations, single thread, 4-vCPU Xeon) measured:

| | per-relation + merged | one CSR + `u8` relation array |
|---|---|---|
| forward bytes | 28.8 MB (16.0 MB without merged) | 13.8 MB |
| build, forward + reverse | 104 ms | 40 ms |
| BFS, 1 / 2 / 3 / 5 of 5 relations | 2.0 / 4.9 / 7.2 / 5.3 ms | 5.4 / 8.7 / 9.3 / 6.0 ms |
| weighted row sums, 1 / 2 / 3 / 5 of 5 | 0.88 / 1.8 / 2.7 / 1.7 ms | 3.0 / 5.0 / 4.4 / 2.2 ms |

Masked traversal and SpMV are 1.1–3.4× faster per relation, which is the hot path; the cost is about 2× storage and a build that stays well inside the §12 `refresh()` budget. Revisit if the memory cap (not speed) becomes the binding constraint; dropping the stored merged CSR is the first lever.

Every per-relation CSR has a full `id_space + 1` offsets array, so offsets cost 8·(N+1)·(R + 1) bytes per direction regardless of edge count (1,000 relations over 10M nodes would need 80 GB forward). `GraphCsr::build_within` computes this (`GraphCsr::offsets_bytes`) and refuses the build before allocating when it exceeds the caller's budget; the adapter passes its memory cap (§14 item 17). Sparse per-relation offsets are the longer-term lever.

Alignment: every segment starts on a 64-byte boundary when loaded.

Every segment is validated on load, before any unchecked slice is formed: alignment and lengths, `offsets[0] = 0`, offsets monotonic, `offsets[id_space] = M`, every target `< id_space`, and `id_map` sorted. The check is O(N + M), which is cheap next to the detoast. The segment decoder has a fuzz target (§12). Each segment is encoded as a 16-byte little-endian header (magic `WRSG`, format, kind, element width, element count) followed by its payload, so a decoder can reject a segment of the wrong kind or width before reading it.

### 5.4 Build pipeline

`warren.refresh(name, promote bool DEFAULT true, label text DEFAULT NULL)`:

Before the build, `refresh` checks that the caller is a member of the owner role (§7) and locks the graph's `warren.graphs` row `FOR UPDATE`, so refreshes of one graph run one at a time, and re-reads `current_version` under that lock.

1. Pin one snapshot for the whole build: `CommandCounterIncrement()`, then `PushActiveSnapshot(GetTransactionSnapshot())`, popped on the normal path. Transaction or subtransaction abort releases it on error. Both queries run as **read-only** SPI cursors (`SPI_cursor_open_with_args(…, read_only = true, …)`) under that snapshot, in Postgres's read-only transaction state (*M5:* so a function they call cannot write either; an integrity guard, not a sandbox, §7). Under READ COMMITTED, non-read-only SPI statements would each take a fresh snapshot, and `node_query` and `edge_query` could disagree. The queries execute with the graph owner's privileges and under the `search_path` captured from the creator by `create_graph` (`graphs.search_path`), so a caller cannot redirect their unqualified names (`SetUserIdAndSecContext`, as `REFRESH MATERIALIZED VIEW` does; see §7 for both identity switches in `refresh`). No locks are taken on source tables beyond the `AccessShareLock` the queries themselves take; pgGraph, by contrast, takes `SHARE NOWAIT` on every source. Record `pg_current_wal_lsn()`. Each query's result columns are checked against the definition on every refresh (ids of the graph's `id_type`, then `text` relation and `float8` weight per the flags), so a source whose types changed since `create_graph` fails with `datatype_mismatch`.
2. Stream `node_query` through the cursor into `warren-core`. Sort the external IDs in memory, and merge-walk against the previous promoted version's `id_map` (if any) to assign dense IDs per §5.2. NULL node ids are ignored.
3. Stream `edge_query`. Map `src` and `dst` to dense IDs in `warren-core` (binary search over the typed `id_map`, never text keys or SQL joins). Map `rel` through `rel_dict`, appending new relations. Edges with a NULL, unknown or tombstoned endpoint, or with a NULL relation (if `has_relations`) or NULL weight (if `weighted`), are dropped and counted in `dropped_edges`. If `directed = false`, emit both directions with the same weight (a self-loop once).
4. Build the forward CSR with no comparison sort, since dense IDs are already known: counting-sort the edge list by `src`, then sort each row by `(rel, dst)` and deduplicate parallel edges within the row (sum weights if weighted; count in `deduplicated_edges`; see §14). Split per relation, and build the merged CSR if there are multiple relations (a target reached through several relations appears once, weights summed); with one relation, its CSR is the all-relations CSR. Free the edge list, then build the reverse CSR by **transposing** the forward CSR: count in-degrees, prefix-sum, scatter while scanning sources in order. That is O(N + M), and rows come out sorted by source.
5. Insert a new `versions` row with `staged = true` and the version's stats (`seq` is the graph's highest plus one), then write `id_map`, `tombstones`, `rel_dict` and the CSR segments in chunks under it, and record their total `bytes`.
6. If `promote`: set `graphs.current_version`, set `staged = false`, and delete promoted versions beyond `retain_versions` (never delete a version that has named vectors bound to it unless `warren.drop_version` is called explicitly — see §7).

Because the build runs under the caller's snapshot, **it sees the caller's uncommitted writes**. This is the mechanism behind staged evaluation (§8).

Memory discipline: a 100M-edge graph (~1.2 GB forward + ~1.2 GB reverse) should build in under 4 GB of backend RAM. The edge list stores endpoints at the target width (20 B per weighted edge below 2³² nodes: `u32` source, target and relation, `f64` weight; the adapter passes the edge count to `EdgeIntake::reserve` so it is allocated once). The counting sort scatters it into rows one column at a time, freeing each input column as it goes, so the peak is about 28 B per weighted edge; the rows are freed once the per-relation CSRs exist, and the merged CSR is built from those. Measured peak (M1, 10M weighted edges over 1M nodes, scaled ×10): **~2.9 GB for one relation with reverse**, within the cap. With several relations the stored merged CSR doubles the result (~5.8 GB weighted with reverse, ~2.6 GB unweighted), so a weighted multi-relation graph of 100M edges exceeds the cap; the lever is not storing the merged CSR (§5.3). All graph-sized buffers use `try_reserve`. The edge list is reserved once, up front, for the planner's row estimate of `edge_query` (the cursor's top plan node), so it does not overshoot its final size by geometric growth; a reservation that fails (a stale overestimate) is ignored and the list grows as rows arrive. An external-sort spill is added only if M2 measurements show it is needed.

### 5.5 Backend-local cache

On first use of a `version_id` in a backend:
- read the chunks via SPI (*M2:* the loader exists — `cache::load` decodes a version's segments over the segment-store port and validates them together through `GraphCsr::from_parts` and `GraphVersion::new`; `diff`, `compact` and `refresh`'s base version use it);
- copy them into contiguous, 64-byte-aligned segment buffers allocated with the **Rust allocator**, not `palloc`. Compute worker threads read these buffers (§6.9), `Arc` ownership makes eviction during a scan safe, and the allocator gives explicit control over alignment;
- validate the segments (§5.3);
- keep them in an `Arc<VersionData>` in a backend-local map keyed by `version_id`.

Set-returning functions hold an `Arc` clone, so `warren.evict` during a scan is safe. `graphs.current_version` is re-read at the start of each top-level call, through a saved SPI plan, so a promoted build is picked up by the next statement. Because the read uses the statement's snapshot, REPEATABLE READ callers keep seeing the version that was current when their snapshot was taken. Vectors are cached the same way, keyed by `vector_id` (*implemented, M4:* `vector::cache`; a vector row is never updated — writing a name again deletes the old row and inserts a new one — so an id names one immutable vector, and a cache miss first prunes cached vectors whose rows are gone; `warren.evict` frees a graph's cached vectors as well as its versions and counts both). `warren.prewarm(name)` loads eagerly for pooled connections. `warren.evict(name)` frees. *Implemented (M3, `cache::open`):* every query function resolves its graph through one saved plan (`GraphDefinition::find`), checks reader access (§7), takes the version asked for (which must belong to the graph) or the current one (an error, `object_not_in_prerequisite_state`, if the graph was never promoted), and looks it up in a backend-local `version_id → Arc<LoadedVersion<E>>` map (type-erased, one entry per version whatever its id type). Versions are immutable, so entries never go stale; a cache miss first prunes cached versions that no longer exist (dropped with their graph, by `drop_version` or by retention, or rolled back), so memory held by dropped versions is reclaimed at the next load. `warren.prewarm(name, version DEFAULT NULL) RETURNS bigint` loads a version (readers only) and returns its id; `warren.evict(name) RETURNS bigint` frees this backend's cached versions of the graph and returns how many, and needs no privilege since it only frees the caller's memory. Segment buffers are ordinary `Vec`s from the Rust allocator; the 64-byte alignment is not yet enforced. *Memory (M3, PR #4 review):* loading a graph's **current** version also drops that graph's other cached versions (a scan holding an `Arc` keeps its copy; an explicit `version :=` call reloads), so a pooled backend holds one copy per graph after promotions rather than one per retained version. A size-bounded LRU (a `warren.max_cache_memory` GUC) is M7 work.

Cold load from TOAST runs at a few hundred MB/s — around a second per GB. v1.1 moves the cache into DSM (`dsa`) so backends share one copy and compute workers can see it (§10).

## 6. Compute layer

This is what turns `pg_warren` from a traversal index into a platform for whole-graph analytics. Its governing principle is **SQL all the way down**: there is no vertex-program language, no runner, and no control-flow DSL. Control flow is written in whatever procedural language the user already has (PL/pgSQL, PL/Python); the engine provides whole-graph primitives that each cost one SQL call per superstep; and the one place a per-row expression is needed accepts SQL expression syntax. The engine is a set of fast kernels behind ordinary functions, nothing more.

The justification: once every operation is a whole-vector primitive, a procedural loop costs microseconds per *superstep* (one C-level call), while the superstep itself costs tens to hundreds of milliseconds at 10M nodes. The loop is free. What is *not* free is per-row work that leaves the engine, and the compute model below is organised around how much of that each kind of program needs.

### 6.1 Three tiers of expressiveness

| Tier | Vertex state | Messages & combine | Apply | Speed at 10M nodes | Status |
|---|---|---|---|---|---|
| **1 — numeric, in-engine** | named vectors (fixed-width numeric) | semiring SpMV | `vector_map` (SQL expression, compiled) | ms per superstep | v1 |
| **2 — gather in-engine, apply in SQL** | any table (text, arrays, jsonb, composites) | `gather`: engine fan-out over CSR, fixed set of combiners incl. `collect` | any SQL | seconds per superstep | v1 |
| **3 — WASM** | bytes | user-supplied combine and compute in a sandboxed module | user code in the engine loop | ~10–50× slower than tier 1 | v1.2+ |

Tier 1 covers the linear-algebra family: PageRank, components, label propagation, SSSP, scores, embedding propagation, spectral methods. Tier 2 is general Pregel with SQL as the vertex language: the expensive part of any Pregel program is the M edge operations, and those stay in CSR; the N per-vertex applies run as set-based SQL with the full type system. Tier 3 exists for state that must be per-vertex and bytes-shaped, and is expected to be rare once §6.6 exists.

Two engine-level decisions keep the tiers open and are **non-negotiable from M1**:

- `warren-core` message buffers are generic over a `Message` trait, with fixed-width numeric as the fast path and variable-width `(offsets, bytes)` as the general path. An `f64`-only core would force a rewrite to reach tiers 2 and 3. **Implemented (M4):** `compute::Message` is implemented by every vector element type (stored as a plain array) and by `Bytes` (packed as end offsets plus bytes); `MessageBuffer<M>` is append-only; before each reservation it charges the capacity it is about to reserve, so its charge is always its reserved capacity. **M5:** `Message::KIND` names the kind (an element dtype or bytes) for combiner type errors.
- The gather kernel accepts messages from, and returns results to, SQL relations as well as named vectors. Named vectors are an optimisation for tier 1, not the only interface.

### 6.2 Vectors and matrices

A `warren.vector` is a dense array of length `id_space`, indexed by dense ID, bound to a graph version. `width > 1` makes it a row-major N×d matrix — the natural home for a pgvector embedding column pulled into graph-aligned memory.

```sql
-- materialise from a query (via id_map); missing nodes get fill
warren.vector_from_query(graph text, name text,
                         query text,        -- SELECT <id>, <value or float[]> ...
                         dtype text DEFAULT 'f64', fill float8 DEFAULT 0,
                         version bigint DEFAULT NULL) RETURNS bigint;  -- vector_id

warren.vector_const(graph text, name text, value float8, dtype text DEFAULT 'f64') RETURNS bigint;
warren.vector_degree(graph text, name text, direction text DEFAULT 'out', rels text[] DEFAULT NULL) RETURNS bigint;
warren.vector_dense_id(graph text, name text) RETURNS bigint;   -- each node's own dense ID; seeds label propagation

-- read back
warren.vector_values(graph text, name text, version bigint DEFAULT NULL)
  RETURNS TABLE (node_id <id_type>, value float8);              -- width=1
warren.matrix_rows(graph text, name text, version bigint DEFAULT NULL)
  RETURNS TABLE (node_id <id_type>, row float8[]);              -- width>1; cast to vector for pgvector
warren.vector_topk(graph text, name text, k int, descending bool DEFAULT true)
  RETURNS TABLE (node_id <id_type>, value float8);
warren.vector_stats(graph text, name text) RETURNS jsonb;      -- min/max/mean/sd/l2/nnz
warren.id_map(graph text, version bigint DEFAULT NULL)
  RETURNS TABLE (node_id <id_type>, dense_id bigint, live bool);

warren.drop_vector(graph text, name text, version bigint DEFAULT NULL) RETURNS void;
```

Vectors are persisted in `warren.vectors` / `vector_chunks` so results survive the session and can be materialised into user tables with ordinary `INSERT ... SELECT`. Storage cost: 8 bytes × id_space per f64 vector; 80 MB for 10M nodes.

Decisions made in M4 (`warren-core::compute`):
- `dtype` is `f32 | f64 | i64 | bool` (SQL `real`, `double precision`, `bigint`, `boolean`). A vector holds `id_space × width` elements, row-major, `width ≥ 1`.
- Every vector and kernel work buffer of graph size is charged to a `ComputeMemory` budget (the domain side of `warren.max_compute_memory`) before it is reserved with `try_reserve`; a charge past the limit is an error and charges nothing. The one exception is `Vector::from_values`, which adopts a buffer the caller has already allocated (a vector loaded from `warren.vector_chunks`) and charges its capacity afterwards. Per-task scratch bounded by the expression size or the vector width (at most a few thousand elements per thread) is not charged.

Decisions made in M4 (`pg_warren::vector`, `pg_warren::compute_sql`):
- **Signatures as implemented.** `vector_from_query(graph, name, query, dtype DEFAULT 'f64', fill DEFAULT 0, version DEFAULT NULL) RETURNS bigint`; `vector_const(graph, name, value, dtype DEFAULT 'f64', version DEFAULT NULL) RETURNS bigint`; `vector_degree(graph, name, direction DEFAULT 'out', rels DEFAULT NULL, dtype DEFAULT 'f64', version DEFAULT NULL) RETURNS bigint`; `vector_dense_id(graph, name, dtype DEFAULT 'i64', version DEFAULT NULL) RETURNS bigint`; `vector_values(graph, name, id_type anyelement, version DEFAULT NULL) RETURNS TABLE (node_id anyelement, value float8)`; `matrix_rows(graph, name, id_type anyelement, version DEFAULT NULL) RETURNS TABLE (node_id anyelement, row float8[])`; `vector_topk(graph, name, k int, id_type anyelement, descending DEFAULT true, version DEFAULT NULL) RETURNS TABLE (node_id anyelement, value float8)`; `vector_stats(graph, name, version DEFAULT NULL) RETURNS jsonb`; `id_map(graph, id_type anyelement, version DEFAULT NULL) RETURNS TABLE (node_id anyelement, dense_id bigint, live bool)`; `drop_vector(graph, name, version DEFAULT NULL, if_exists DEFAULT false) RETURNS void`. The functions returning external ids have no id argument to take the result type from, so, like `components` (§9.3), they take a type witness: `warren.vector_values('fraud', 'score', NULL::uuid)`. Every compute function takes a trailing `version DEFAULT NULL` (§7): vector names resolve among the vectors bound to that version, and vectors written are bound to it.
- **Persistence.** A vector's bytes are its elements in row-major order, little-endian (`bool` one byte, 0 or 1), with no header (dtype, width and length are `warren.vectors` columns), chunked like segments (`CHUNK_SIZE`); encoding streams chunk by chunk and decoding reserves the whole buffer up front (`compute::encode`, `compute::Decoder`), so persistence never holds a second copy. Every write is a new `warren.vectors` row; writing a name again replaces the old row, and the replaced vector's chunks cascade. A vector is written on every call that produces it: simple and transactional, at the cost of WAL per superstep (an unlogged or deferred write path is a later optimisation, to be measured).
- **Transient vectors (decided in M4 review, §14 item 18).** Every vector-writing function (`vector_from_query`, `vector_const`, `vector_degree`, `vector_dense_id`, `spmv`/`spmm`'s `y`, `vector_map`'s `y`) takes `persist bool DEFAULT true`, just before `version`. With `persist := false` the result is a *transient* vector: kept only in this backend's memory, keyed by (graph, version, **current role**, name), so roles sharing a backend (`SET ROLE`, a connection pooler, a `SECURITY DEFINER` function) can neither see, shadow, replace nor drop each other's (decided in M4 re-review; `warren.evict` frees only the current role's), never written to `warren.vectors` (no WAL, no TOAST, no vacuum), and it returns NULL instead of a vector id. Reads resolve a name to the current role's transient vector first, then the persisted one, in every function (`vector_values`, `vector_map`'s columns, `explain_map`, `spmv` operands, …). Writing a persisted name drops this session's transient vector of that name, and writing a transient name deletes the persisted vector of that name (under the §7 ownership rule); `drop_vector` drops the transient one if there is one. Transient vectors are **not transactional**: a ROLLBACK (or a failed subtransaction) neither removes one written in it nor restores what it replaced, and a persisted vector it deleted comes back while the transient one remains, which then shadows it. They do not pin their version against retention (§7), are invisible to other sessions, and are freed at session end, by `warren.evict` (which counts them), by `drop_vector`, and when a later cache load finds their version dropped. They are not charged to any budget between calls (as cached vectors are not); each call that reads one is charged for it. Iterative programs keep scratch vectors transient and persist only results; the §6.7 library does so.
- **Tombstoned rows hold zero** (`false`) in every vector, whatever produced it (fill, constants, kernel results), and the rows read back, ranked or summarised are the live nodes only: `vector_values`, `matrix_rows`, `vector_topk`, `vector_stats` and `vector_reduce` skip tombstones (`compute::reduce_live`, `compute::summarise`, `compute::top_k`), so a deleted node is never a minimum. `vector_norm_diff` reads every row, so a node live on one side and tombstoned on the other contributes its value.
- **`vector_from_query`** runs the query as the caller, under the caller's `search_path`, as a read-only cursor in Postgres's read-only transaction state (*M5:* so a VOLATILE function it calls cannot write either, 25006; an integrity guard, not a sandbox, §7), wrapped as `SELECT q.id::<id_type>, q.value::<dtype's type>[[]] FROM (<query>) q(id, value)`. It is first prepared (`SPI_prepare`: parsed, analysed, rewritten; not planned or run) to read its column types, and must be one SELECT, VALUES or TABLE (`syntax_error` otherwise) of two columns whose first casts *implicitly* to the graph's id type (`datatype_mismatch` otherwise), so ids are never rounded: `int` ids work on a `bigint` graph, `double precision` ids do not. Values convert by Postgres's own `::` casts (`2.5` is 3 in an `i64` vector, as in SQL). An array value makes a matrix row; every row must have the same width (an empty query gives width 1). Nodes without a row get `fill` (cast as SQL casts `double precision`); NULL, unknown and tombstoned ids are skipped; two rows for one node are a `cardinality_violation`; a NULL value follows `vector_map`'s rule (§6.3: `NaN` in a float vector, 22004 otherwise).
- `vector_degree` counts distinct neighbours, as `warren.degree` does, and defaults to `f64` so `pr / NULLIF(deg, 0)` and `1.0 / deg` stay in `double precision`; `vector_dense_id` defaults to `i64`.
- `vector_stats` returns `{count, min, max, mean, sd, l2, nnz}` over the elements of live rows; `sd` is the sample standard deviation (as `stddev`), combined pairwise (Chan et al.), so it can differ from `stddev` in the last bit; undefined statistics are `null`, and non-finite ones the text `double precision` reads (`"NaN"`, `"Infinity"`), since JSON has no such numbers.
- **Access.** Functions that write vectors (`vector_*` constructors, `spmv`, `spmm`, `vector_map`) need reader access, like every compute function. Replacing or dropping a persisted vector is subject to the ownership rule of §7 (creator or owner). A write records `created_by`, the writing role (§5.1 `warren.vectors`).
- **Memory accounting.** `warren.max_compute_memory` (kB, default `4GB`, minimum `64kB`, `USERSET`) bounds what one call holds: each vector it reads is charged to the call's budget once (whether or not it was cached), as are the vectors and work buffers it allocates; going over is `configuration_limit_exceeded` (53400) before allocating, and allocator failure is `out_of_memory` (53200). Cached vectors between calls are not charged to any call's budget.

### 6.3 Tier 1 — semiring operations and `vector_map`

```sql
-- y = A ⊗ x over a semiring. A is the graph (optionally transposed / relation-masked / weighted).
warren.spmv(graph text, x text, y text,
            semiring text DEFAULT 'plus_times',   -- <add>_<mult>, GraphBLAS names; see below
            transpose bool DEFAULT false,
            rels text[] DEFAULT NULL,
            weighted bool DEFAULT NULL,           -- NULL = use weights if present
            mask text DEFAULT NULL, mask_complement bool DEFAULT false,
            replace bool DEFAULT false,           -- with a mask: false keeps y outside the mask; true sets it to the ⊕ identity
            accumulate text DEFAULT NULL)        -- NULL = overwrite y; else a binary op: y = y ⊙ result
                                                 --   plus | min | max | times | first | second
  RETURNS void;

-- Y = A ⊗ X for a width-d matrix X (neighbour aggregation of embeddings is one call)
warren.spmm(graph text, x text, y text, semiring text DEFAULT 'plus_times', ...) RETURNS void;

-- elementwise: y = <SQL expression over vector names and literals>, evaluated per node
warren.vector_map(graph text, y text, expr text) RETURNS void;
--   e.g. warren.vector_map('kg', 'pr', '0.15 + 0.85 * pr_next / NULLIF(deg, 0)')
--        warren.vector_map('kg', 'live', 'CASE WHEN is_account = 1 OR deg <= 50 THEN 1 ELSE 0 END')
warren.vector_reduce(graph text, x text, op text) RETURNS float8;   -- sum | max | min | l1 | l2 | nnz
warren.vector_norm_diff(graph text, a text, b text, norm text DEFAULT 'l2') RETURNS float8;
```

**`vector_map` accepts SQL expression syntax and nothing else.** The engine parses `expr` with Postgres's own parser against a synthetic row whose columns are the graph's vector names. If the expression lies in the *compiled subset* — arithmetic, comparisons, boolean operators, `CASE`, `NULLIF`, `COALESCE`, `LEAST`/`GREATEST`, and a whitelist of math functions (`abs`, `sqrt`, `exp`, `ln`, `log`, `power`, `floor`, `ceil`, `round`, `sign`) over numeric vectors and literals — it is compiled to a native closure and run across threads at tier-1 speed. Otherwise it is evaluated row-wise through the executor's expression machinery (`ExecEvalExpr`, JIT where available), roughly 100–500 ns per row: slower, but still correct and still SQL. The planner-visible contract is one language everywhere with graceful degradation, never "unsupported syntax". `EXPLAIN`-style introspection (`warren.explain_map(expr)`) reports which path an expression takes.

**Naming follows the GraphBLAS C API** (as exposed by OneSparse), so users moving between the two find the same vocabulary.
- A semiring is named `<add>_<mult>`.
- ⊗ is applied as `⊗(a_ij, x_j)`, so `first` takes the edge weight and `second` takes the input vector value.
- v1 semirings: `plus_times`, `min_plus`, `max_plus`, `max_times`, `min_times`, `lor_land` (alias `or_and`), `any_pair`, `plus_pair`, `plus_first`, `plus_second`, `min_first`, `min_second`, `max_second`.
- A mask is a value mask with GraphBLAS semantics: an entry is selected when it casts to boolean true, meaning any non-zero value. NaN is non-zero, so it is selected. pg_warren vectors are dense, so `replace` (the "no entry" state outside the mask) is represented as the ⊕ identity of the semiring: 0 for `plus`, +∞ for `min`, −∞ for `max`, false for `lor`. `mask_complement` and `replace` correspond to GraphBLAS descriptors `c` and `r`; `transpose` corresponds to `t0`.
- An unweighted graph acts as if every weight were 1.

Semirings are implemented generically in `warren-core` over `(⊕, ⊗, identity)` for f64, i64, and bool element types, and for f32 too, so `aggregate_neighbours` works on f32 embeddings (A.5). With `A[i][j]` the weight of the edge `i → j`, `transpose = false` computes `y = A ⊗ x` (`y_i` combines the out-neighbours' values), pulled row by row over the forward CSR; `transpose = true` computes `y = Aᵀ ⊗ x` (`y_i` gathers from the in-neighbours, as PageRank does), pulled over the reverse CSR. Without a reverse CSR, `transpose = true` scatters (pushes) over the forward CSR instead: one thread, plus an `id_space × width` scratch charged to the memory budget. The pull form parallelises by rows with no write conflicts, which is why `build_reverse` defaults to true. Both forms fold each row's entries in ascending dense-ID order, so they agree bit for bit.

Decisions made in M4 (`warren-core::compute`):
- A relation selection makes `A[i][j]` the sum of the edge's weights over the selected relations (1 when unweighted), which is what the merged CSR holds for all relations. `weighted := true` on an unweighted graph uses weight 1.
- Edge weights are cast to the element type as SQL casts `double precision`: `i64` rounds half to even and fails out of range, `f32` fails on overflow or underflow, `bool` is `weight ≠ 0`. A weight is cast only if `⊗` reads it (`second` and `pair` do not).
- `min` and `max` use SQL order (`NaN` above every number, `-0 = 0`, ties keep the first operand), like `LEAST`/`GREATEST`. `any` takes the row's first entry, so results are deterministic. Float kernels otherwise use IEEE arithmetic (infinities are values, not errors); `i64` overflow is an error (`bigint out of range`). The exception keeps dense "no entry" values stable across supersteps: in the semiring kernels, the `i64` identity of `min` (`i64::MAX`) and of `max` (`i64::MIN`) stands for ±∞ and absorbs `plus` (`MAX + w = MAX` under `min`, `MIN + w = MIN` under `max`), so repeated integer `min_plus`/`max_plus` (hop counts, SSSP) leave unreached nodes at the identity rather than failing. They also absorb `times` by a weight above zero (`MAX · w = MAX` under `min`, `MIN · w = MIN` under `max`, for `w > 0`), so `min_times`/`max_times` supersteps keep unreached nodes too; times by a weight of zero or less treats the identity as an ordinary `bigint` (`MAX · 0 = 0`, `MAX · −1 = −MAX`; `MIN · −1` and `|w| ≥ 2` overflow), since ±∞ · w has no `bigint` value there. Every other overflow still errors. Floats need no exception, since `∞ + w = ∞`.
- Accumulation follows GraphBLAS: a row of `A` with no entry leaves `y_i` unchanged under `accumulate`, and without it writes the `⊕` identity. `accumulate` accepts any GraphBLAS binary operator name (`first | second | pair | plus | times | min | max | lor | land | any`; `first` keeps `y`, `second` takes the result). The mask is a width-1 vector of any element type, also for `spmm`, where it selects whole rows.
- `vector_map`'s compiled subset is a typed IR (`compute::Expr<T>` for each element type, built by the adapter from Postgres's analysed tree with every implicit cast made explicit). It is evaluated in parallel chunks, column at a time, under a mask of the elements SQL would evaluate: `CASE`, `COALESCE`, `AND` and `OR` evaluate an argument only where Postgres's executor would, so `CASE WHEN x = 0 THEN 0 ELSE 1/x END` does not fail. Operators, functions and casts follow Postgres's `float.c` and `int8.c`, including their errors (division by zero, overflow, underflow, `sqrt`/`ln`/`power` domain errors), so the compiled path raises the executor fallback's error. When several elements fail, the error is the first failing element's in row-major order. Postgres has no `boolean`↔`real`/`double precision` casts; the IR defines them as for `bigint`.
- Vectors hold no NULL. Where an expression is NULL, a float vector stores `NaN` (so `pr / NULLIF(deg, 0)` works), and an `i64` or `bool` vector raises `null value cannot be stored in a … vector` (22004); the executor fallback applies the same rule (`compute::Value::NULL`). `NaN` read back is `NaN`, never NULL.
- Broadcasting: the result's width is the widest operand's; literals and width-1 vectors repeat across each row (`emb / deg` normalises rows); any other width is an error.
- Throughput (`benches/compute.rs`, 100K nodes, 1M edges, `plus_times`, 4-vCPU Xeon): pull over the forward or reverse CSR 0.38–0.42 G edges/s on one thread and 1.2 G edges/s on four; push without a reverse CSR 0.22 G edges/s (single-threaded). The §12 PageRank row is measured in M5.
- `vector_reduce` reads elements as `double precision` (`bool` as 0/1); `max` and `min` of no elements are NULL; `nnz` counts `NaN`. Reductions and `vector_norm_diff` combine fixed-size chunks in order, so results do not depend on `warren.threads`.

Decisions made in M4 (`pg_warren::compute_sql`):
- **`spmv`, `spmm`** take exactly the arguments above plus a trailing `version DEFAULT NULL`. They read `y`'s old values only where the product keeps them — outside the mask without `replace`, or under `accumulate` — and then `y` must have `x`'s dtype; otherwise (and when there is no `y` yet) `y` starts as `x`'s dtype and width filled with the `⊕` identity, GraphBLAS's empty vector in dense form. Unknown relation names in `rels` are skipped, as in traversal (§9.3).
- **`vector_map(graph, y, expr, version DEFAULT NULL)`** replaces `y` with the expression's value. `expr` is first parsed alone as `SELECT <expr>` and must be one target with no other clause (`syntax_error` otherwise), then analysed under the caller's `search_path` as `SELECT (<expr>) FROM (VALUES (NULL::<type>, …)) AS v(<vector names>)`, whose columns are the vectors bound to the version (transient first, §6.2) that the expression's column references name (every field of each `ColumnRef` in the raw tree, so the row's width does not grow with the vectors bound), typed by dtype; aggregates, window functions, set-returning functions and subqueries are `feature_not_supported`. The result's dtype is the expression's type, with `integer`/`smallint` cast to `bigint` and `numeric` to `double precision` in the analysed tree (so both paths cast alike); other types are `datatype_mismatch`. A matrix column has its element type, and the result's width is the widest operand's (so `0.7 * emb + 0.3 * emb_nbr` over `f32` rows is `f64`; cast it, `(…)::real`, to keep `f32`).
- **The compiled subset** is decided by function OID (`OpExpr.opfuncid`, `FuncExpr.funcid`): Postgres's `float8`, `float4`, `int8`, mixed `float48`/`float84` and `int84`/`int48` arithmetic and comparisons, `bool` comparisons, unary minus and plus, `%` on `bigint`, `abs`, `sqrt`, `exp`, `ln`, `log`/`log10`, `power`/`pow`/`^`, `floor`, `ceil`/`ceiling`, `round(float8)`, `sign`, and the casts among `float8`, `float4`, `int8`, `int4`, `int2` and `bool`; plus `Const`, `Var`, `CASE` (searched and simple), `NULLIF`, `COALESCE`, `LEAST`/`GREATEST`, `AND`/`OR`/`NOT`. `integer` and `smallint` values compile as `bigint` (exact for values, comparisons and casts), but their arithmetic, which overflows at a smaller range, does not. A `numeric` constant compiles where it is cast to `double precision` or `real` (the usual `0.85 * x`), converted from its decimal text with correct rounding as `numeric_float8` does; a constant that cast would reject falls back. A user's own operator or function never compiles, whatever its name. Everything else is evaluated by the executor.
- **The executor fallback** compiles the same analysed tree once with `ExecInitExpr` (parent: a standalone `ResultState` over a fresh `EState`), and evaluates it per element over a virtual tuple of the synthetic row (referenced columns set, others NULL; a width-1 vector repeats across each row), resetting the per-tuple memory and checking for interrupts every 4,096 elements. It JIT-compiles when the planner would for a plan of cost `elements × cpu_operator_cost` (`jit`, `jit_expressions`, `jit_above_cost`, with `jit_optimize_above_cost` and `jit_inline_above_cost` for the optimisation flags). It runs on the main thread only, as the caller (never as the extension owner) and under the caller's `search_path`, so names a called function resolves when it runs (SQL bodies, PL/pgSQL statements, `regclass` casts) resolve as in the caller's own SQL. A NULL result is stored as on the compiled path.
- **Agreement.** The compiled path and the executor give the same bytes and the same errors (SQLSTATE and message) — tested on 300 random expressions over random vectors with zeros, `-0`, NaN, infinities and extremes, with the executor forced by a test-only hook, and on division by zero, float and bigint overflow, logarithm and square-root domain errors and NULL into `i64`.
- **`explain_map(graph, expr, version DEFAULT NULL) RETURNS jsonb`**: `{"path": "compiled" | "executor", "reason": …, "dtype": …}`; for the executor the reason names the first operation outside the subset (`"function numeric_add is not in the compiled subset"`). Every Appendix A expression takes the compiled path (tested).
- `vector_reduce(graph, x, op, version DEFAULT NULL)` reduces the live rows. `vector_norm_diff(graph, a, b, norm DEFAULT 'l2', a_version DEFAULT NULL, b_version DEFAULT NULL)` reads `a` at `a_version` and `b` at `b_version` (each the current version if NULL) and refuses (`invalid_parameter_value`) versions a compaction separates.

A tier-1 program is a PL/pgSQL function over these primitives. The canonical shape:

```sql
CREATE FUNCTION my_pagerank(graph text, damping float8 DEFAULT 0.85, tol float8 DEFAULT 1e-6)
RETURNS int LANGUAGE plpgsql AS $$
DECLARE i int; delta float8;
BEGIN
  PERFORM warren.vector_degree(graph, 'deg');
  PERFORM warren.vector_const(graph, 'pr', 1.0);
  FOR i IN 1..100 LOOP
    PERFORM warren.vector_map(graph, 'msg', 'pr / NULLIF(deg, 0)');
    PERFORM warren.spmv(graph, 'msg', 'gathered', transpose := true);
    PERFORM warren.vector_map(graph, 'next', format('%s + %s * gathered', 1 - damping, damping));
    delta := warren.vector_norm_diff(graph, 'next', 'pr', 'l1');
    PERFORM warren.vector_map(graph, 'pr', 'next');
    EXIT WHEN delta < tol;
  END LOOP;
  RETURN i;
END $$;
```

`spmv` is Pregel's gather, `vector_map` is apply, keyword arguments (`semiring`, `mask`, `transpose`, `rels`) are the vocabulary of the model. The library in §6.7 is written exactly this way so its source doubles as templates.

### 6.4 Tier 2 — `gather`: messages from SQL, apply in SQL

```sql
-- Fan a per-node message out over the graph and combine at the receiver, in-engine.
warren.gather(graph text,
              messages text,                 -- a vector name, a relation name, or SQL: SELECT <id>, <msg of any type> ...
              id_type anyelement,            -- witness: NULL::<the graph's id type>
              value_type anycompatible,      -- witness: NULL::<the combiner's result type>, e.g. NULL::text[] for collect over text
              combine text DEFAULT 'collect',-- collect | count | sum | min | max | first | concat | bool_or | bool_and
              direction text DEFAULT 'in',   -- messages travel along edges; 'in' = receive from in-neighbours
              rels text[] DEFAULT NULL,
              mask text DEFAULT NULL, mask_complement boolean DEFAULT false,
              version bigint DEFAULT NULL)
  RETURNS TABLE (node_id anyelement, value anycompatible);   -- value is <msg_type>[] for 'collect'
```

The engine reads the message relation once through SPI, maps IDs to dense IDs, fans out over CSR with the chosen combiner in-engine (variable-width messages via the `Message` trait), and returns one row per receiving node. `collect` returns the array of received messages and makes the operation fully general: any Pregel `compute()` is `collect` followed by SQL.

A tier-2 superstep is therefore a `gather` plus an `UPDATE`/`INSERT ... SELECT` over a state table:

```sql
-- entity resolution: each node accumulates the set of identifiers seen in its neighbourhood
CREATE TEMP TABLE state (node_id uuid PRIMARY KEY, idents text[]);
INSERT INTO state SELECT id, ARRAY[id::text] FROM fraud_nodes;
FOR i IN 1..10 LOOP
  UPDATE state s
     SET idents = (SELECT array_agg(DISTINCT x) FROM unnest(s.idents || g.value) x)
    FROM warren.gather('fraud', 'SELECT node_id, unnest(idents) FROM state', NULL::uuid, NULL::text[]) g
   WHERE s.node_id = g.node_id;
END LOOP;
```

Cost is dominated by N rows through the executor per superstep — seconds at 10M nodes — not by the M edge operations, which stay in-engine. Per-vertex set accumulation of this kind grows memory as O(N × neighbourhood); it is the right tool for local neighbourhoods and the wrong tool for community-wide state, which is what §6.6 is for.

Decisions made in M5 (`warren-core::compute::gather`):
- Senders. A receiver gets one copy of each message of each of its *senders*: its distinct neighbours in `direction` over the selected relations. `in` = in-neighbours `{s : s → r}` (the orientation of `spmv(transpose := true)`), `out` = out-neighbours (`transpose := false`), `both` = their union. An edge in several selected relations or in both directions makes its sender a sender once; edge weights are not read. The SQL oracle is therefore `SELECT DISTINCT` (sender, receiver) pairs joined with the messages, grouped by receiver.
- Messages are `(dense id, message)` pairs in any order; a node may send several messages or none. The adapter maps the message relation's ids to dense ids; a dense id outside the id space is an error. Fixed-width messages are any vector element type; anything else is serialised to bytes (`compute::Bytes`).
- Collect order: ascending sender dense id, then a sender's messages in the order they were read. Every combiner folds in this order, so results (float sums included) are bit-identical for any `warren.threads`. Receivers with no message, or outside the mask, return no row.
- Combiner types follow SQL: `collect`, `count` (→ `bigint`) and `first` take any message; `sum`, `min`, `max` take `f32 | f64 | i64` (`min`/`max` in SQL order, `NaN` greatest; `i64` sum overflow is `bigint out of range`, where SQL's `sum(bigint)` would widen to `numeric`); `bool_or`/`bool_and` take `bool`; `concat` takes bytes (the adapter sends `text`/`bytea` as bytes). Any other pairing is `combine := <c> does not accept <kind> messages`, raised before any work. `min`/`max` of text would need collation, so they are not done in-engine.
- Kernel: messages are counting-sorted by sender into an outbox (`id_space + 1` offsets plus a sender-ordered copy). Receivers are pulled in parallel tasks of 4,096 over the CSR rows holding their senders: forward rows for `out`, reverse rows for `in`. Without a reverse CSR, `in` scatters the senders' forward rows into a per-receiver inbox of `(receiver, sender)` pairs (one thread, cancellation checked every 65,536 rows), charged to the memory budget like the outbox. Every buffer is charged its reserved capacity (`Message::growth`, exact geometric reservations). Result parts reserve the budget ahead in blocks (4 KiB doubling to 1 MiB, exact near the limit) and settle to their reserved capacity at the end of each task: charging every message to the shared atomic made `collect` slower on more threads.
- Output: per receiver, ascending, a `count`, one message (fixed-width element or bytes), or for `collect` the receiver's messages with each message's boundaries kept (end offsets into packed storage), streamed to SQL row by row.
- Throughput (`benches/gather.rs`, 100K nodes, 1M edges, every node sending, 4-vCPU Xeon): pulled, `sum` of f64 14 ms on one thread and 8 ms on four, `collect` of f64 19 ms and 8 ms, `collect` of 8-byte messages 46 ms and 18 ms; pushed (no reverse CSR) 30–70 ms.

Decisions made in M5 (`pg_warren::compute_sql::gather`, the SQL binding):
- The result types come from two witnesses, as `components` takes one (§9.3): `id_type` (`anyelement`, the graph's id type) and `value_type` (`anycompatible`, a separate polymorphic family, the combiner's result type). One signature cannot return `T[]` for `collect` and `bigint` for `count` from a run-time argument; a wrong witness is an error naming the right one (42804).
- Message source (§14 item 13): text that is not an identifier or `schema.identifier` is a query, never a vector. An identifier is a vector bound to the version if one has exactly that name (every live node sends its value; width 1), else a relation (`TABLE <name>`); a name that is both a bound vector and a relation on the caller's `search_path` is an error (42725), since vector names are shared by the graph's writers and one could otherwise redirect another user's `gather`. `TABLE <name>` always means the relation. A relation or query must return two columns, `(id, message)`; the id must cast implicitly to the id type; it runs as the caller under the caller's `search_path` as a read-only cursor in Postgres's read-only transaction state, as `vector_from_query` and the build's source queries do (§7: an integrity guard, not a sandbox; what still works under it is listed there). A data-modifying CTE is refused (0A000), as is a write through a function the query calls (25006); a relation resolves on the caller's `search_path` and is read with the caller's privileges; the caller must be a reader of the graph. Rows with a NULL, unknown or tombstoned id, or a NULL message, send nothing; a node may send several messages (one row each).
- Result types (`codec.rs`), with the message's base type: `count` → `bigint`; `first`, `min`, `max` → the message type; `sum` of `smallint`/`integer`/`bigint` → `bigint` (overflow is an error, where SQL's `sum(bigint)` widens to `numeric`), of `real` → `real`, of `double precision` → `double precision`; `bool_or`/`bool_and` → `boolean`; `concat` of `text`/`varchar`/`char` → `text`, of `bytea` → `bytea`; `collect` → `array_agg`'s type: `T[]`, or for array messages the same array type with one more dimension, built by `accumArrayResultAny`, so ragged arrays fail as `array_agg` fails (2202E). To collect sets of different sizes, send their elements as separate messages (`SELECT id, unnest(idents) ...`).
- `smallint`, `integer`, `bigint`, `real`, `double precision` and `boolean` messages take the fixed-width path; `concat` reads text or bytea payloads; any other type travels as its binary send form and is rebuilt by its receive function (a type without binary I/O is Postgres's error). `sum`/`min`/`max` of other types (`numeric`, `text`) and `bool_*` of non-booleans are refused before any work.
- `gather` reads (it needs read access to the graph, writes nothing) and runs within one `warren.max_compute_memory` budget: the messages, the kernel's buffers and the result are charged to it. The result streams: the SRF's iterator owns the kernel's result (charged until the last row) and builds each row's datums when the executor asks for it, so only the current row is palloc'd. `concat` reads text and bytea as varlena bytes, never re-encoding.

### 6.5 Spectral analysis

```sql
-- top-k eigenpairs by magnitude (or smallest, for Laplacian) via Lanczos with full reorthogonalisation
warren.eigen(graph text, k int,
             matrix text DEFAULT 'adjacency',   -- adjacency | laplacian | normalized_laplacian | random_walk
             which text DEFAULT 'largest',      -- largest | smallest
             rels text[] DEFAULT NULL, weighted bool DEFAULT NULL,
             symmetrize bool DEFAULT true,      -- (A + Aᵀ)/2 for directed graphs; false → Arnoldi (v1.1)
             tol float8 DEFAULT 1e-8, max_iter int DEFAULT NULL,
             vectors_prefix text DEFAULT NULL,  -- store eigenvectors as <prefix>_1..k
             version bigint DEFAULT NULL,
             dtype text DEFAULT 'f32',          -- f32 | f64: the Lanczos and eigenvector element type (§14 item 8)
             persist bool DEFAULT true)         -- false: the eigenvectors are transient vectors
  RETURNS TABLE (i int, eigenvalue float8, residual float8);
```

**Implemented (M6, `pg_warren::spectral_sql`)** with that signature. The precision argument is `dtype`, the stored vectors' element type, because `precision` is an SQL keyword. Eigenvectors are persisted by default (they are results, and `distance` reads them in later statements); `persist := false` keeps them as transient vectors of the session. Owners and readers may call it; replacing stored eigenvectors follows the vector rules of §7 (only their writer or the graph owner). `eigen` is `PARALLEL UNSAFE` (it persists by default); `distance` reads and stores nothing, a fresh solve for `spectral` included, so it is `STABLE PARALLEL RESTRICTED` and runs in parallel mode (§6.9). The call runs within `warren.max_compute_memory` and polls interrupts like the compute functions.

Only SpMV is needed from the graph; with full reorthogonalisation Lanczos keeps its whole basis, so its state is m vectors of length `id_space` (m the basis size below) plus an m² projected matrix. For N = 10⁷, k = 50 (m = 101) that is ~8 GB in f64 — use f32 Lanczos vectors by default with f64 accumulation, or cap k and document. Implement in `warren-core` directly (~300 lines with reorthogonalisation); use `faer` or `nalgebra` for the small dense tridiagonal eigenproblem. Do not pull in ARPACK bindings in v1. SVD of the (rectangular, directed) adjacency via Lanczos on AᵀA is a straightforward extension. Full spectra are out of scope; k ≤ a few hundred is the design envelope.

**Implemented (M6, `warren_core::spectral`).**

- *Operators* (`GraphMatrix`) are applied matrix-free with PlusTimes SpMV over the selected relations, weighted or not. `S = (A + Aᵀ)/2` from the forward and reverse CSR (pushed over the forward CSR without one); `symmetrize = false` skips the second SpMV and is refused (`NotSymmetric`) unless a Freivalds check (`A·r = Aᵀ·r` for a seeded random `r`, relative tolerance 10⁻⁹) passes, so an undirected graph halves the cost. `D` holds the row sums of `S` (a self-loop counts once). `laplacian = D − S`, `normalized_laplacian = I − D^-1/2 S D^-1/2`; `random_walk = D^-1 S` runs on the similar `D^-1/2 S D^-1/2` and returns eigenvectors `D^-1/2 φ`, renormalised (its residual is that of `φ`). The normalised operators refuse a node with an edge and a non-positive degree.
- *Inactive nodes.* The eigenproblem is over the **active** nodes, those with at least one edge of `S`. Tombstoned nodes have no edges, and isolated live nodes (including nodes with no edge in the selected relations) are left out too: each would add an exact 0 to both Laplacians, a whole eigenspace of them in a graph with many isolated nodes. Every eigenvector is exactly zero at inactive nodes, and `k` may not exceed the number of active nodes.
- *Lanczos* with full reorthogonalisation and thick restart: the basis holds m = `max(2k + 1, 20)` vectors (at most the dimension, ARPACK's default); when full, the Rayleigh–Ritz pairs of the dense `H = QᵀMQ` (nalgebra `SymmetricEigen`, `std` feature only) decide convergence, else the basis is compressed to the k wanted Ritz vectors plus one per converged pair, clamped to between (m − k)/4 and (m − k)/2 more (ARPACK's `nev + min(nconv, np/2)` adjustment; the lower clamp keeps clustered spectra, such as a bipartite graph's ±λ pairs at small k, from stalling), and extended from the last residual. Each step subtracts the three-term recurrence's `β·q_{j−1}` and `α·q_j`, then makes one classical Gram–Schmidt pass against the whole basis (the recurrence is the first pass of "twice is enough"); the first step after a restart or a random vector, whose product couples to every kept vector, makes two full passes. The norm and the `α` removal are fused into those passes, so a step sweeps the basis twice. The basis is stored in blocks of 4,096 rows, vector by vector within a block, so the passes stream contiguous segments. `H`'s entries are the reorthogonalisation coefficients, so it is the projection onto the basis as stored. A breakdown (invariant subspace) continues from a fresh seeded random vector orthogonal to the basis.
- *`which`*: `largest` is largest magnitude (`λ` before `−λ` on ties), `smallest` is smallest algebraic. No spectral shift is used for `smallest`: a Krylov space of `σI − M` is that of `M`, so the Ritz values are the same; shift-and-invert would need linear solves.
- *Convergence*: a pair has converged when `|β·s_last| ≤ tol·max|θ|`; `max_iter` counts operator applications (default 100·m, raised to k) and `Eigen::converged` says whether the test passed in time. The `residual` returned is `‖Mx − θx‖₂`, computed explicitly for the eigenvector as stored.
- *Precision* (§14 item 8): `f32` basis and eigenvectors by default, every sum in `f64`; `f64` is the option. With `f32` vectors the explicit residual floors near 10⁻⁷·‖M‖, the rounding of the stored vectors.
- *Determinism* (Appendix A.2): start and restart vectors are counter-based functions of (seed, stream, dense id), every reduction sums fixed-size blocks in block order, SpMV folds rows in dense-id order, and eigenvectors are signed so that their largest-magnitude entry is positive. Results are bitwise identical across runs and thread counts.
- *Memory*: the basis, the operator's `f64` work vectors (three, four for the normalised operators) and degrees, and the k returned vectors are charged to `ComputeMemory`; workers poll the cancellation flag per block and the main thread polls the interrupts before every step.
- *Measured against ARPACK* (`bench/eigen.py`: SciPy 1.18 `eigsh`, seeded start vector, default `ncv`, on the harness's `random` fixture of 100K nodes and 1M undirected edges, 2M non-zeros; k = 20, tol = 10⁻⁶, f32 vectors; a shared 4-vCPU box, so seconds are noisy, steps are not; `results/eigen/`):

  | | ARPACK matvecs (s) | before: steps, 1 / 4 threads | now: steps, 1 / 4 threads |
  |---|---|---|---|
  | adjacency, largest | 1,020–1,313 (15–31 s) | 1,405, 31.0 / 13.9 s | 1,037, 9.9 / 3.7 s |
  | normalised Laplacian, smallest | 1,406–1,477 (10–65 s) | 1,702, 40.6 / 16.1 s | 1,342, 13.1 / 5.1 s |

  ARPACK's count varies by a quarter with its start vector; ours is fixed by the seed. Eigenvalues agree to 10⁻⁹. The step count fell with the restart retention, the time per step (now ~9.5 ms on one thread, ~3.7 ms on four) with the two-sweep orthogonalisation. A basis of 3k + 1 cuts the steps further (800 and 1,159) at 1.5× the memory; it is not the default. A uniform random graph's spectrum is a dense bulk, about the slowest case. Scaled linearly to 10M nodes and 100M edges, a step is ~0.4 s on four threads, so k = 20 projects to ~8–9 min on this box against the §12 target of 10 min on eight cores; M7 measures it.
- *Limitation*: from one start vector, Lanczos finds one direction per eigenspace, so the copies of a repeated eigenvalue appear only through rounding or a breakdown restart. A graph with c connected components has Laplacian eigenvalue 0 with multiplicity c; compute on the largest component when that matters. Block Lanczos would lift this.
- *Follow-up: block Lanczos* (not now). Lanczos with a block of b vectors finds up to b copies of an eigenvalue and replaces b SpMVs per block step by one SpMM, which reads the CSR once per block (a bandwidth win at 10⁷ nodes). It needs more operator applications for simple spectra, a block QR per step, and block-aware restarts and convergence tests. Single-vector Lanczos already needs about as many operator applications as ARPACK, so block Lanczos is deferred until repeated eigenvalues matter in practice (components, symmetric structures) or the SpMV bandwidth binds at the M7 scale.

### 6.6 Community-stateful computation: contraction, project, lift

Per-vertex models — Pregel included — go wrong on **community-stateful** computations, where state belongs to a group of vertices rather than to each one: every member ends up carrying a copy, memory grows as O(N × community size), and the workaround is to elect leader vertices and route through them. The scalable form is two-level: state lives once per community, vertices carry only a membership label, and the computation alternates between levels. This design supports that in two stages.

**Stage 1 (v1, no engine support needed).** A community is a `GROUP BY` on a label vector produced by tier 1. The community table holds state once per community; broadcasting back to members is a join; member-to-member messaging uses `gather`. State is stored once, which is already better than Pregel.

**Stage 2 (v1.1; API shape fixed now).** Three operations borrowed from multigrid methods make the two-level pattern native, fast and recursive:

```sql
-- Quotient graph: one node per distinct label, edges aggregated (weight = edge count or summed weight).
-- Creates a derived graph in the catalog whose id_map is the label values and which stores the
-- membership matrix P (N × K, one non-zero per row) linking it to its parent version.
warren.contract(graph text, label_vector text, out_graph text,
                weighted bool DEFAULT true, drop_self_loops bool DEFAULT false,
                version bigint DEFAULT NULL) RETURNS bigint;             -- quotient version_id

-- vertex → community aggregation (Pᵀ·x, an SpMV): sum | mean | max | min | count
warren.project(graph text, x text, out_graph text, y text, agg text DEFAULT 'sum') RETURNS void;

-- community → vertex broadcast (P·y, an SpMV)
warren.lift(out_graph text, y text, graph text, x text) RETURNS void;
```

With these, a community-stateful computation is a tier-1 or tier-2 program *on the quotient graph* with `project`/`lift` at the boundaries. Because `contract` produces a real graph version, it recurses: contracting the quotient gives hierarchical communities, which is exactly Louvain/Leiden and multilevel partitioning — library algorithms rather than special engines. The membership matrix is one more sparse relation in CSR; nothing new is needed in `warren-core`.

Design obligations imposed now:

1. The catalog's notion of a graph gains a second **source**: derived by contraction, referencing a parent version and a label vector, rather than only built from SQL queries. Versioning, staging, retention and vectors work identically; the quotient's `id_space` is the label range; a `parent_version` column on `warren.versions` records the link.
2. Vectors are bindable to quotient graphs, and `project`/`lift` validate that the parent and quotient versions correspond. Trivial given stable IDs.

### 6.7 Algorithm library (v1)

Shipped as PL/pgSQL functions over the primitives (readable, copyable), each returning a named vector and parameterised by `rels`, `weighted`. Their intermediate vectors are transient (`persist := false`, §6.2); only the named result is persisted:

- `warren.pagerank(graph, out, damping, tol, max_iter, seed := NULL)` — seed gives personalised PageRank.
- `warren.components_vector(graph, out)` — min-label propagation (`min_second` SpMV); also available as the traversal SRF.
- `warren.label_propagation(graph, out, seed, max_iter)` — community / semi-supervised.
- `warren.sssp_vector(graph, out, source)` — min_plus SpMV from a source; all-nodes distances.
- `warren.degree_vector(graph, out, direction, rels)`.
- `warren.aggregate_neighbours(graph, x, out, agg DEFAULT 'mean', hops DEFAULT 1, rels)` — mean/sum/max over neighbour rows of an N×d matrix; the "one-layer GNN" primitive for smoothing or propagating pgvector embeddings.
- `warren.triangles(graph, out)` — per-node triangle count via masked SpMM (needs sorted targets).
- v1.1, on top of §6.6: `warren.louvain(graph, out, levels)` and `warren.leiden(...)`.

Decisions made in M5 (`pg_warren::compute_sql::library`, one `.sql` file per function):
- Signatures: `pagerank(graph, out, damping DEFAULT 0.85, tol DEFAULT 1e-6, max_iter DEFAULT 100, seed DEFAULT NULL, rels, weighted, version) RETURNS int`; `components_vector(graph, out, rels, version) RETURNS int`; `label_propagation(graph, out, seed DEFAULT NULL, max_iter DEFAULT 100, mask DEFAULT NULL, rels, version) RETURNS int`; `sssp_vector(graph, out, source anyelement, rels, weighted, max_iter DEFAULT NULL, version) RETURNS int`; `degree_vector(graph, out, direction DEFAULT 'out', rels, version) RETURNS bigint`; `aggregate_neighbours(graph, x, out, agg DEFAULT 'mean', hops DEFAULT 1, rels, direction DEFAULT 'out', version) RETURNS void`. Iterative functions return the iterations they ran; `degree_vector` returns the vector id.
- Each pins its version once (`version`, else the current one) and passes it to every call, so a concurrent `refresh` cannot split a run across versions. Intermediates are transient vectors named `_warren_<function>_<part>`, dropped at the end, and also when the run fails or is cancelled (an `EXCEPTION WHEN OTHERS OR query_canceled` handler drops them and re-raises: one subtransaction per call); only `out` is persisted. The `_warren_` prefix is reserved: no vector of such a name can be persisted (42939, `reserved_name`), so an intermediate can never replace, and so delete, a persisted vector, and the library refuses it for `out` and its input vectors. Constants (`1 / N`, `damping`, a seed's sum, the dangling mass) enter expressions as constant vectors, never as formatted text, so `extra_float_digits` cannot round them. Every function sets `search_path = pg_catalog, pg_temp`. Tests assert that no `vector_map` they run falls back to the executor.
- `pagerank` follows NetworkX: out-weight normalisation (`plus_first` row sums; edge count when unweighted), dangling rank and the teleport to the personalisation (uniform, or `seed / sum(seed)`), uniform start, stop when the L1 change < live nodes × `tol`.
- `components_vector` and `label_propagation` are min-label propagation (`min_second` both ways with `accumulate := 'min'`, Jacobi updates) until no label changes; `label_propagation` takes a seed vector and a mask (unselected nodes neither send nor take labels). It is deterministic min-propagation, not majority-vote LPA.
- `sssp_vector` is Bellman-Ford with `min_plus` along edges until no distance falls; by default at most the live node count of iterations, after which distances still falling mean a negative cycle reachable from the source, an error (22023), as NetworkX raises; an explicit `max_iter` just stops there. An unknown source is an error (22023); unreachable nodes are `Infinity`.
- `triangles(graph, out, rels DEFAULT NULL, persist DEFAULT true, version DEFAULT NULL) RETURNS bigint` is a kernel, not PL/pgSQL: semiring products alone would need SpGEMM or an N×N dense matrix. It counts each node's triangles in the undirected simple graph under `rels` (directions and self-loops ignored, as NetworkX): a symmetric adjacency is built once (charged; no reverse CSR needed), then each node sums sorted-row intersections with its neighbours, in parallel: a merge for rows of similar length, else a binary search of the shorter row's values in the longer (more than 16× longer), so a hub costs about its degree × log, not its degree squared. Cost about `Σ_u Σ_{v∈N(u)} min(d_u, d_v) · log max(d_u, d_v)`; a task checks its cancellation flag every 65,536 row entries it visits, so a hub's row cannot outlast a cancel (§6.9).
- `aggregate_neighbours` iterates `hops` times over distinct neighbours (weights are not read); a node with no neighbours gets NaN for `mean` (SQL `AVG` of nothing), the `⊕` identity for `sum` (0) and `max` (−∞); `mean` needs a float matrix and keeps its dtype.

### 6.8 Tier 3 — WASM vertex programs (v1.2+)

For state that must be per-vertex and bytes-shaped with logic that neither SQL nor contraction expresses well, accept a WASM module exporting `init(vid)`, `compute(vid, messages_ptr, len)`, `combine(a, b)`; the engine runs it in a tight loop per superstep with `wasmtime`, messages marshalled through linear memory via the same `Message` trait as tier 2. Expect roughly 10–50× slower than tier 1. Deferred until tiers 1–2 and §6.6 have shown which programs users cannot express.

### 6.9 Parallelism

- A Postgres backend is single-threaded and **no thread other than the main one may call any Postgres function** (palloc, elog, SPI, catalog access, …). Pure compute over extension-owned buffers (CSR, vectors, message buffers) is safe on worker threads. `warren-core` therefore takes plain slices and a thread count, and uses `rayon` with a dedicated pool sized by the `warren.threads` GUC (default: half the cores, min 1). The binding layer does all Postgres interaction before and after each kernel call — including, for `gather`, reading the message relation through SPI on the main thread before fan-out and emitting result rows after.
- *Implemented (M4, `compute_sql::settings`):* `warren.threads` (`USERSET`, 1–1024, default half of `available_parallelism`, at least 1) sizes one compute pool per backend, built on first use and rebuilt when the setting changes. The pool's threads are started with every signal blocked, so Postgres's handlers (which touch backend state non-atomically) only ever run on the main thread. The main-thread poll is `pg_warren::interrupts::PostgresInterrupts`, shared with traversal; a cancel marked while a kernel runs stops it with `query_canceled` (tested). `Workers::run` catches the unwind of the ERROR that `CHECK_FOR_INTERRUPTS` raises in the poll (rayon's scope does), waits for the job to see its cancellation flag and return, then resumes the unwind. That is sound only because nothing calls into Postgres between the catch and the resume; no Postgres call may be added to that window. The test marks the cancel from the main-thread poll, so no worker thread ever touches Postgres state.
- Interrupt handling: the main thread polls `CHECK_FOR_INTERRUPTS()` between kernel calls and sets a cancellation flag that worker loops check every ~64K vertices. **Implemented (M4):** `compute::Workers::run` runs a kernel as a job on the pool while the calling (main) thread waits, polling the `interrupts::Interrupts` hook (shared with traversal) every millisecond. When a poll says stop, or unwinds (pgrx raising the ERROR), a drop guard raises the job's `Cancellation` flag and the rayon scope waits for the job to stop before the error propagates. The scope catches that unwind (`halt_unwinding`), waits for the job, then resumes it; this is the one place pg_warren holds a caught Postgres ERROR, and it is sound only because nothing between the catch and the resume calls into Postgres. A panicking job marks itself finished from a drop guard, so the scope returns at once and re-raises the panic, which pgrx turns into an ERROR. Worker tasks check the flag at the start of each task of at most 4,096 rows (4,096 elements for `vector_map`, 65,536 for reductions); the push scatter checks it every 65,536 rows, as do `gather`'s sequential sorts (outbox, inbox). Use `pgrx::check_for_interrupts!()`. pgrx's `pg_sys` FFI is `#[pg_guard]`ed, so an ERROR raised inside it unwinds the Rust frames (destructors run, `RefCell` borrows release). Never call unguarded FFI.
- Errors are raised only through pgrx (`ErrorReport` with a standard SQLSTATE; detail and hint as needed), never by calling `pg_sys` error functions directly. Nothing in `pg_warren` uses `catch_unwind` around SPI; pgrx already installs the boundary at each `#[pg_extern]`.
- Memory for vectors, message buffers and work buffers is allocated with the Rust allocator, not `palloc`, and accounted against a `warren.max_compute_memory` GUC; exceeding it raises an error rather than OOMing the server (*M4:* per call, §6.2). Graph-sized buffers are reserved with `try_reserve`, so allocator failure is also an error (`53200`), not an abort.
- v1.1: a **compute background worker** that owns the DSM-resident graph and runs jobs submitted by backends via a shared queue, so compute does not compete with query backends and large jobs can outlive a client session. Results land in `warren.vectors`; `warren.job_status()` reports progress.
- GPU: the semiring/SpMV formulation is what cuSPARSE/GraphBLAST expect; a GPU backend for `warren-core` is a contained future addition.

### 6.10 GraphBLAS: bind or build?

SuiteSparse:GraphBLAS is the mature implementation of the semiring model (masks, SpMV/SpMM, OpenMP parallel). Binding it would save implementing the tier-1 kernels but adds a large C dependency, a second allocator and threading model inside the backend, licence considerations (Apache-2.0 for recent versions, LGPL historically — verify), and it does not cover tier 2's variable-width messages at all. The v1 lean is to **implement the kernels natively in Rust** (SpMV, SpMM, elementwise map/reduce over a handful of semirings, plus `gather`), keeping `warren-core` dependency-free, with the API designed so a GraphBLAS backend could be slotted in for harder operations (masked SpMM, SpGEMM) if benchmarks justify it. Record the decision in §14 once M6 benchmarks exist.

## 7. Versions, retention, and staging

- A graph has zero or more versions. Exactly one is `current` (promoted). Any call that takes `version bigint DEFAULT NULL` operates on `current` when NULL.
- `warren.refresh(name, promote := false)` builds a **staged** version: fully built, loadable, queryable by explicit `version`, but not visible to callers that use the default. `warren.promote(name, version)` makes it current; `warren.drop_version(name, version)` discards it.
- `retain_versions` controls how many promoted versions are kept; staged versions are never auto-deleted. A version with bound (persisted) vectors is not auto-deleted either (a `NOTICE` reports what was retained and why); transient vectors (§6.2) never pin a version, so scratch vectors should be transient or dropped with `drop_vector`. `compact` moves the source's vectors to the compacted version (§5.2), so the source no longer pins and retention deletes it in turn. *Implemented (M2):* every promotion (`refresh(promote := true)` and `promote`) keeps the current version plus the `retain_versions − 1` newest other promoted versions by `seq` (so the current version counts towards the limit and is kept even when it is an older one promoted back), and deletes the rest unless vectors are bound to them; deleting a version cascades to its segments. `drop_version` deletes any version but the current one (vectors included); `drop_graph` deletes the graph and everything built from it.
- Versions share the dense ID space (until `compact`), so any vector bound to version *n* can be read against version *m* for comparison (*M4:* `vector_norm_diff`'s `a_version` and `b_version`, §6.3) — `warren.vector_norm_diff` and `warren.distance` (§8) accept vectors from different versions of the same graph and align by dense ID, treating nodes absent from either side as zero.
- **Access.** A graph exposes topology computed with its owner's visibility, exactly like a materialised view; source-table RLS does not apply to what traversal reveals. Traversal and compute functions therefore require the caller to be a member of the owner or of a role in `graphs.readers`. `warren.grant(name, role)` and `warren.revoke(name, role)` maintain that list. Management functions (`refresh`, `promote`, `drop_*` except `drop_vector`, `compact`, `grant`, `revoke`) require membership in the owner role. **Vector ownership (decided in M4 review):** `warren.vectors.created_by` records the role that wrote each persisted vector. Replacing a persisted vector (writing its name again, from any vector-writing function) or dropping it (`drop_vector`) requires membership in its creator or in the graph's owner role (`insufficient_privilege` otherwise, naming both); a replacement is then the replacer's. So readers share one name space per version but may only replace or drop their own vectors, and owners may replace or drop any. A vector whose creator was dropped is the owner's alone. The check binds the write to the row it checked: the write deletes only that row, by `vector_id`, and any other row of that name (committed by another role since the writer opened the version, or in progress) makes the write fail with `serialization_failure` instead of being deleted unchecked; `drop_vector`, transient replacement and `compact`'s move delete by `vector_id` too (harness test `vector_replace_race`). Readers' vectors still occupy the extension owner's storage, with no per-role quota (v1.1, with the `DROP ROLE` clean-up below). `owner` is set to `current_user` by `warren.create_graph` and no function changes it; ownership transfer is out of scope for v1.
- **Catalog writes and identity switches.** All `warren.*` functions are `SECURITY INVOKER`. Because the tables are revoked from `PUBLIC` (§5.1), each catalog, segment or vector SPI statement runs inside an internal switch to the **extension owner**: `GetUserIdAndSecContext`, then `SetUserIdAndSecContext(extowner, SECURITY_LOCAL_USERID_CHANGE | SECURITY_RESTRICTED_OPERATION)`, then restore. The extension owner is read once per backend from `pg_extension.extowner`, and an `ERROR` inside the switch is undone by (sub)transaction abort. This is preferred over `SECURITY DEFINER` functions because the elevation then covers only extension-written SQL. Schema-qualifying the tables is not enough to make that SQL fixed: operators (`=`, `||`), functions and catalog relations (`pg_roles`, `pg_has_role`) resolve through `search_path`, so a caller could put its own `=` ahead of `pg_catalog` and have it run as the extension owner, or shadow `pg_roles`/`pg_has_role` to pass an access check (both reproduced in review, M2). **Every `warren.*` entry point therefore runs its whole body under `access::entry`, which pins `search_path = pg_catalog, pg_temp`** (`pg_temp` explicitly last) the way `fmgr` applies `SECURITY DEFINER … SET search_path`: `NewGUCNestLevel`, `set_config_option(…, PGC_S_SESSION, GUC_ACTION_SAVE)`, and `AtEOXact_GUC` on return, with (sub)transaction abort restoring it on error. This covers the access checks that run as the caller as well as the elevated statements; the checks also schema-qualify `pg_catalog` objects. The caller's own `search_path` is captured before pinning and used only for the caller's own SQL (a new graph's source queries, checked by `create_graph`). A test calls every function in schema `warren` with a hostile `search_path` and fails if the hostile operator runs or the access checks give way. With `SECURITY DEFINER`, everything the function evaluates would run elevated: user-supplied `node_query`/`edge_query`, `gather` message SQL and `vector_map` executor-fallback expressions. Avoiding that would need a switch back to the invoker at each of those points, which is easier to get wrong.
- **Caller SQL runs read-only — an integrity guard, not a sandbox.** Every read of SQL a user wrote (a graph's `node_query`/`edge_query` during `refresh`, `vector_from_query`'s value query, `gather`'s message query or relation) runs as a read-only SPI cursor with the backend in Postgres's read-only *transaction* state (`XactReadOnly`, set around the read by `access::with_read_only` and restored on return or error; the `transaction_read_only` GUC cannot be set inside a function, so the flag is set directly). A read-only cursor alone checks only its top statement, so this is what stops a VOLATILE function the query calls from writing: such a write fails with 25006, and a data-modifying CTE with 0A000. It is the same guarantee `SET TRANSACTION READ ONLY` gives, not isolation: still possible are INSERT/UPDATE/DELETE on existing temporary tables, `pg_notify`, `ANALYZE`, `LOCK`, assigning a transaction id, `COPY … TO PROGRAM` and `lo_export` for roles privileged to use them, and writes over a separate connection (`dblink_exec`). None of these escalates privilege: caller SQL runs as the caller, and source queries as the graph owner (above), so it can do nothing its role could not do directly. What it guarantees is that a source or message query cannot change the tables a build or kernel is reading.
- `refresh` therefore makes two identity switches: to the **graph owner** while `node_query` and `edge_query` run (§5.4), and to the **extension owner** for the catalog and segment writes. User SQL never runs as the extension owner.
- **Dropped roles.** `owner` and `readers` are `regrole` values, which are bare OIDs with no `pg_shdepend` entries, so `DROP ROLE` does not see them. `refresh`, and every function that checks access, raise an error if the owner role no longer exists. Reader OIDs that no longer resolve to a role are ignored by access checks and pruned by `grant` and `revoke`. A graph whose owner was dropped can still be removed: `drop_graph` lets a superuser drop it. A `DROP ROLE` event trigger that cleans these up is a v1.1 item.
- `warren.diff(name, v1, v2)` returns structural deltas: nodes added/removed, edges added/removed per relation, degree-distribution shift — computed by merging the two CSRs row-wise, O(N + M). *Implemented (M2, `warren_core::version::diff`):* an edge is a (src, dst) pair, so a weight change alone is not counted; relations are aligned by name (later dictionary's order, then relations only in the earlier one); an all-relations delta comes from the merged CSR. The degree shift is per direction (out, and in counted from the forward CSR, so no reverse is needed) over live nodes: a sparse histogram, the means, and the exact two-sample KS statistic (NULL when a side has no live nodes). `diff` refuses when the later id space is smaller or any shared dense ID names a different external ID, which catches comparisons across a compaction boundary. Benchmark: ~25 ms single-threaded at 100K nodes / 1M edges (`benches/version_diff.rs`). *SQL (M2):* `warren.diff(name, v1, v2) RETURNS jsonb`, chosen over rows because the degree histograms nest: `{from, to, nodes: {before, after, added, removed}, relations: [{relation, before, after, added, removed}], all_relations: {…}, out_degree: {mean_before, mean_after, ks_statistic, histogram: [{degree, before, after}]}, in_degree: {…}}`. Owners and readers may call it. It refuses (`invalid_parameter_value`) when a version with `compacted_from` set lies after the earlier and at or before the later of the two by `seq`, naming the compaction, before loading anything.

## 8. Change detection workflow

The motivating scenario: a large knowledge graph in Postgres receives a new corpus. Before committing it, the team wants to measure how much the *global structure* of the graph changes — not just "how many edges were added" but whether the new material reshapes the graph's dominant structure — and gate the commit on that.

Because builds are transactional and see uncommitted writes, this is expressible in one transaction:

```sql
BEGIN;

-- 1. ingest the new corpus into the ordinary nodes/edges tables (uncommitted)
INSERT INTO entities ...;  INSERT INTO relations ...;

-- 2. build a staged version from this transaction's view of the tables
SELECT warren.refresh('kg', promote := false, label := 'corpus-2026-10-17');
--    → returns new version_id, say 42; current is 41

-- 3. structural delta (cheap)
SELECT * FROM warren.diff('kg', 41, 42);

-- 4. spectral comparison (seconds to minutes depending on N, k)
SELECT warren.eigen('kg', 20, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap', version := 41);
SELECT warren.eigen('kg', 20, 'normalized_laplacian', 'smallest', vectors_prefix := 'lap', version := 42);
SELECT warren.distance('kg', 41, 42, method := 'subspace', vectors_prefix := 'lap', k := 20);
--    → principal angles between the two dominant eigen-subspaces, summarised as a scalar

-- 5. decide
--    COMMIT  → corpus and version 42 persist; optionally SELECT warren.promote('kg', 42);
--    ROLLBACK → corpus, version 42 and its vectors all vanish; nothing to clean up.
COMMIT;
```

`warren.distance(graph, v1, v2, method, ...)` provides a small set of comparable scalars:

- `edit` — normalised symmetric difference of edge sets, per relation.
- `degree` — distance between degree distributions (Wasserstein / KS).
- `spectral` — distance between the top-k eigenvalue sequences (requires `eigen` on both versions, or computes it).
- `subspace` — principal angles / projection-Frobenius distance between the spans of the top-k eigenvectors; the most sensitive to structural reorganisation and the least sensitive to volume. Requires stable dense IDs (§5.2); refuses across a compaction boundary.
- `embedding` — mean/max row-wise distance between two N×d matrices (e.g. neighbour-aggregated embeddings before and after), so embedding drift induced by structural change is measurable too.

```sql
warren.distance(graph text, v1 bigint, v2 bigint, method text,  -- edit | degree | spectral | subspace | embedding
                vectors_prefix text DEFAULT NULL, k int DEFAULT NULL,  -- spectral, subspace: <prefix>_1..k
                x text DEFAULT NULL, y text DEFAULT NULL,      -- embedding: matrix x of v1, y (default x) of v2
                compared_rows text DEFAULT 'shared',           -- subspace, embedding: shared | all
                matrix text DEFAULT 'adjacency', which text DEFAULT 'largest', rels text[] DEFAULT NULL,
                weighted bool DEFAULT NULL, symmetrize bool DEFAULT true, tol float8 DEFAULT 1e-8,
                max_iter int DEFAULT NULL, dtype text DEFAULT 'f32',  -- spectral: the operator, and a fresh solve
                graph2 text DEFAULT NULL)                      -- v2 is a version of graph2, aligned by external id
  RETURNS jsonb;   -- {"method", "from", "to", ["graph2"], ...the method's scalars}
```

**Implemented (M6, `warren_core::spectral`, bound by `pg_warren::spectral_sql`).** Each method returns a small struct of scalars, which the binding returns as `jsonb` (§14 item 11); `edit` names each relation. Versions are aligned by dense id; past a version's id space everything is absent.

- `edit_distance` → per relation (by relation index) and overall over (relation, source, target) triples: `added`, `removed`, `union`, and the Jaccard distance `(added + removed)/union`. Weights are ignored. The row-wise merge of the two forward CSRs is `warren.diff`'s (`EdgeDelta::between`), O(N + M).
- `degree_distance` → `wasserstein` (W₁, in degrees) and `ks` between the distributions of distinct out-neighbour counts in the all-relations forward CSR, over live (not tombstoned) nodes; an empty version is a point mass at 0.
- `spectral_distance` → `l2`, `relative` (over the larger norm) and `max` of the in-order difference of two eigenvalue sequences, missing as 0.
- `subspace_distance` → `projection` (`‖P_X − P_Y‖_F = √(k₁ + k₂ − 2Σcos²θ)`), `max_angle` and the principal `angles`, from one block-ordered Gram pass over both vector sets, Cholesky and an SVD of `L_X⁻¹XᵀY L_Y⁻ᵀ` (inputs need only be independent; cosines within 8Kε of 1 count as 1, so identical inputs give exactly 0).
- `embedding_distance` → `mean_l2`, `max_l2`, `mean_cosine`, `max_cosine` (1 − cos, from normalised rows) and `rows` compared, skipping rows zero on both sides.
- `subspace` and `embedding` take a `ComparedRows`: `All` reads every row with absent rows as zero (§7), so added and removed nodes count as change; `Shared` reads only nodes present (non-zero) on both sides. A.2 needs `Shared`: with `All`, added volume moves every eigenvector's mass onto the new rows and outweighs restructuring.
- `spectral` with `vectors_prefix` compares the Rayleigh quotients of the stored `<prefix>_1..k` under `matrix` (their eigenvalues, without solving again); without it, it solves `k` eigenpairs on each version.
- `compared_rows` defaults to `shared`: A.2 compares structure, not volume, and over `all` the added nodes' rows dominate (§8 above).
- Cross-graph comparison (`graph2`, the shadow-schema pattern): `version::align` renumbers the second version into the first's id space by external id (shared nodes keep the first graph's dense ids, the second's own nodes are appended in external-id order, relations match by name, its CSRs are rebuilt), and `spectral::align` moves its vectors' rows the same way; the same functions then run. Both graphs need the same `id_type`. Versions of one graph are refused across a compaction (as `diff` is); across graphs the external ids align them, so a compaction is no boundary there.
- Small-instance A.2 check (unit test): 4 planted communities of 30, 48 nodes ingested; over shared nodes the subspace distance of the 4 smallest normalised-Laplacian eigenvectors is < 0.5 for *volume* and > 1 for *novel* and *bridge*, whose relative order depends on the sizes (it flips with communities of 40 and 64 ingested), so A.2's `novel < bridge` is checked at its own scale in the integration test. The harness test `test/sql/a2_spectral.sql` (6 communities of 40, 80 nodes and 1,600 edges ingested, k = 6, f64) measures volume 0.40, novel 1.27, bridge 1.43 and asserts volume < 0.5 and both others above twice volume; with 400 ingested edges the bridge barely merged its two communities (0.24), which shows the ordering's dependence on the generator's density. The shadow-schema comparison matches the same-graph one within 10⁻⁹ (k = 6, f64, tol 10⁻¹², so that both Lanczos runs converge past the comparison's resolution although their start vectors differ by dense id). After `aggregate_neighbours` (mean, one hop) of the fixture's 64-dim embeddings, `distance('embedding')` over shared nodes gives a mean L2 drift of 0.16 for *bridge*, 0.13 for *volume* and 0.02 for *novel* (whose entities barely link to the base graph), and the test asserts *bridge* highest; its margin over *volume* is only about a quarter at this scale.

Design consequences that this workflow imposes, all already reflected above: stable dense IDs; staged versions; transactional segment storage; eigenvectors stored as versioned vectors; alignment-by-dense-ID in all cross-version operations; retention that does not silently delete a version somebody is comparing against.

Operational note: a long transaction holding an uncommitted corpus plus a staged build holds back vacuum on the source tables. For very large corpora, the alternative is to ingest into a *shadow* schema or partition, point a second graph definition at it, build and compare across graphs with the same `id_type` (cross-graph `distance` aligns by external ID through both `id_map`s), and then swap. Both patterns should be documented.

## 9. Traversal functions

### 9.1 The query paradigm and the seam

`pg_warren` deliberately has no query language. The model is the PostGIS/pgvector one: functions that take node IDs in and return relations out, composed with ordinary SQL on either side. A graph query in a `pg_warren` system therefore has two forms, and the user chooses between them per query:

- **Shallow / small-frontier:** write a recursive CTE or explicit self-joins over the real `nodes`/`edges` tables. Fully transactional, planner-optimised, no staleness. This is the default and covers most queries.
- **Deep / large-frontier:** replace the CTE with a single `warren.*` call and join its result back to the real tables for filtering and projection. Snapshot semantics (§7).

The switching criterion is **frontier size, not hop count.** Three hops at average degree 10 is a thousand intermediate rows and belongs in SQL; three hops at degree 1,000 is a billion and belongs in the engine. A rule of thumb for users: if `EXPLAIN` of the CTE shows the recursive term producing more than ~10⁵–10⁶ rows, or if the query is iterative at all, move it above the seam.

What makes the seam cheap to cross is that both forms produce a relation keyed by external ID, so moving a query from one side to the other changes one clause, not the surrounding query. Keeping that property is a design constraint on every traversal function.

The one place the functions as originally scoped were too blunt is multi-relational graphs: real knowledge-graph queries are rarely "everything within k hops" but "follow *this* relation, then *that* one, with a predicate at each step". Chaining `warren.neighbours` per hop through lateral joins expresses that, but it re-enters the executor and materialises a set at every hop — exactly the cost model the engine exists to avoid. Hence `warren.walk` (§9.2), which keeps a typed chain inside the engine for its whole length. It is still a function; it is not a language.

### 9.2 `warren.walk` — typed path patterns

A walk is an ordered list of steps. Each step is a relation (or set of relations), a direction, an optional node filter applied to the nodes reached by that step, and a hop range for variable-length segments. The engine executes the whole chain over CSR without leaving the backend, returning the end frontier or full paths.

```sql
CREATE TYPE warren.step AS (
    rels      text[],      -- NULL = any relation
    direction text,        -- 'out' | 'in' | 'both'
    filter    anyarray,    -- allowed nodes after this step (external IDs); NULL = no filter
    min_hops  int,         -- default 1
    max_hops  int          -- default = min_hops; >min gives a variable-length segment
);

warren.walk(graph text, start anyarray, steps warren.step[],
            mode text DEFAULT 'ends',            -- 'ends' | 'paths' | 'count'
            distinct_ends bool DEFAULT true,
            max_results bigint DEFAULT NULL,
            reverse bool DEFAULT false,          -- execute the chain from the other end (see below)
            version bigint DEFAULT NULL)
  RETURNS TABLE (end_id <id_type>, path <id_type>[], rels text[]);   -- path/rels NULL unless mode='paths'

-- convenience constructor so steps read well in SQL
warren.s(rels text[], direction text DEFAULT 'out', filter anyarray DEFAULT NULL,
         min_hops int DEFAULT 1, max_hops int DEFAULT NULL) RETURNS warren.step;
```

Example — papers cited by anything within 1–3 citation hops of a root, written by currently active people, at UK institutions:

```sql
SELECT i.*
FROM warren.walk('kg', ARRAY[:root], ARRAY[
        warren.s(ARRAY['cites'], 'out', NULL, 1, 3),
        warren.s(ARRAY['authored_by'], 'out', (SELECT array_agg(id) FROM people WHERE active)),
        warren.s(ARRAY['affiliated_with'], 'out', (SELECT array_agg(id) FROM institutions WHERE country = 'GB'))
     ]) w
JOIN institutions i ON i.id = w.end_id;
```

Semantics and implementation notes:

- Each step's filter array is converted to a dense-ID bitmap once per call; filters are tested before a node is admitted to the next frontier, so predicates on intermediate nodes prune the traversal rather than post-filter it.
- Within a variable-length segment the engine runs a level-synchronous BFS restricted to that step's relations, emitting nodes at every depth in `[min_hops, max_hops]` into the next step's start frontier. Visited sets are per segment (a node may legitimately be revisited by a later step).
- `mode = 'ends'` keeps only the end frontier and is the fast path (no predecessor storage). `'paths'` keeps per-step predecessor arrays and enumerates paths on return; it must be bounded by `max_results` and documented as potentially exponential. `'count'` returns the number of end nodes (or paths) without materialising them.
- `reverse` executes the chain from its last step backwards (directions flipped, filters applied in reverse order) and reports the nodes that would have been *starts*, with `start` then interpreted as the set of allowed ends. This is the manual version of the selectivity-aware start choice a graph planner would make automatically: when the far end of a pattern is rarer than the near end, start there.
- No variable binding and no joins within a pattern in v1: a walk is a chain. Cycles and "A and B share a C" are expressed in SQL by intersecting the results of two walks on `end_id`, which the planner handles well because both sides are already small. See §10 for what a pattern language would add beyond this.
- `warren.step[]` is a **stable intermediate representation**. Any future sugar — a parsed pattern string, say — must compile to it rather than bypass it, so that the engine has exactly one traversal entry point to optimise.

Decisions made in M3 (`warren-core::traversal::walk`, dense ids in and out):

- The code calls a step's `min_hops..max_hops` its **hop range**; a "variable-length segment" above is a step whose hop range spans several lengths. "Segment" keeps its storage meaning.
- **Hops are walks**, as in a recursive CTE with a depth column: nodes and edges may repeat. A step ends on every node some walk of `k ∈ [min_hops, max_hops]` hops reaches, not only on nodes whose shortest distance is in range (a `2..2` step from `a` with `a→b→c` and `a→c` ends on `c`; a start can be its own end through a cycle). Below `min_hops` each depth keeps its own node set; from `min_hops` on, one visited set per step is exact, because a node first admitted at depth `j ≥ min` already led everywhere a later visit would, within `max`.
- A step's filter holds every node a hop of the step enters and the node the step hands on. Starts are not filtered. `min_hops = 0` is allowed: the step may hand on the node it started from, if the filter allows it. `HopRange` requires `min ≤ max ≤ 1024`; the cap stops a cyclic graph doing work at absurd depths.
- **Walk identity** (for `count` without distinct ends, and for `paths`): a walk is a sequence of hops, each a `(relation, neighbour)` pair, with each step's hops its own. An edge in two selected relations is two walks; an edge read both ways by one relation under `both`, or a self-loop, is one hop; the same nodes split differently between steps are different walks (as with Cypher's per-pattern bindings). Repeated starts count once. A walk of no steps ends where it starts.
- `ends`: with `distinct_ends`, the fast path (merged rows, no predecessors), streamed as found (`walk_ends_each`); without, each end once per walk, streamed after the walks are counted.
- `count` is exact, never approximate: distinct ends, or walks counted level by level with checked `u64` arithmetic (an error past `u64::MAX`), without materialising them. `max_results` does not apply to it.
- `paths`: with `distinct_ends`, one path per end, read back from how the fast path first reached each node at each level, streamed as found (`walk_paths_each`). Without, every walk, enumerated depth-first over the walk counts' levels pruned backwards to nodes that can still finish, so every branch yields a path. Paths carry each hop's relation index. The number of walks can be exponential in the hops; the adapter always passes `max_results` (§14 item 15).
- `reverse` runs the steps last to first with directions flipped; reversed step `m` hops only *from* nodes in `filter_m` and hands on only nodes in `filter_{m-1}`, and the starts must be in the last filter, so every walk found is a forward walk read backwards. It reports would-be starts; its paths read forward, from the reported node to an allowed end. A property test checks every mode, forwards and reversed, against brute-force enumeration.
- `last_stats()` covers the whole walk: `nodes_visited` counts each node admitted to a level of a step (each step's starting nodes, then each node a hop admits), so a filter never raises it; `edges_traversed` counts row entries scanned; `truncated` means `max_results` stopped the walk.

Decisions made in M3 (`pg_warren::traversal_sql`, the SQL binding):

- `warren.step` is the composite `(rels text[], direction text, filter text[], min_hops int, max_hops int)`. A composite type cannot have a polymorphic attribute, so a step holds its filter's external ids as text in their id type's output format; `walk` reads them back with the graph's id type's input function (a malformed id raises `invalid_text_representation`), skipping ids that are not nodes of the version, as filters do everywhere (§9.3). A NULL `direction` is `out`, a NULL `min_hops` 1, a NULL `max_hops` equal to `min_hops`.
- `warren.s(rels text[], direction text DEFAULT 'out', filter "any" DEFAULT NULL, min_hops int DEFAULT 1, max_hops int DEFAULT NULL) RETURNS warren.step`: `filter` is `"any"` for the same reason as `bfs`'s `node_filter`. The `jsonb → warren.step[]` cast (`warren.steps(jsonb)`, §14 item 14) takes an array of objects with optional keys `rels`, `direction`, `filter`, `min_hops`, `max_hops` and the same defaults; anything else (not an array, a non-object element, an unknown key, a `rels` or `filter` that is neither an array nor JSON null) is `invalid_parameter_value`, since silently widening a malformed step would return more rows than asked for.
- `warren.walk(graph, start anyarray, steps warren.step[], mode DEFAULT 'ends', distinct_ends DEFAULT true, max_results DEFAULT NULL, reverse DEFAULT false, version DEFAULT NULL) RETURNS TABLE (end_id anyelement, path anyarray, rels text[], count bigint)`. A fourth column, `count`, carries `mode = 'count'`'s answer in its single row (the other columns NULL); in the other modes it is NULL. A count above `bigint`'s range is `numeric_value_out_of_range`, never clamped. `path` and `rels` are set only in `paths` mode. With `reverse`, `end_id` is the reported would-be start, `path[1]`.
- A walk that reports a row per walk (`paths`, or `ends` without `distinct_ends`) is always bounded: `max_results` defaults to 100,000 for it, and a larger `max_results` than the `warren.max_walk_paths` GUC (default 10,000,000, `USERSET`) is refused with `program_limit_exceeded` (§14 item 15). Stopping at the default is truncation, with the `NOTICE`.
- The domain's walks are callback-driven (`walk_ends_each`, `walk_paths_each`) rather than cursors, so `warren.walk` collects its rows (within the row limit above) before returning the first; SQL `LIMIT` does not shorten a walk.
- `last_stats()` reports `function = 'walk'`; a step filter lowers `nodes_visited` (Appendix A.4, tested).
- Oracle: each mode, forwards and reversed, against a chain of recursive CTEs with a depth column (one per step, tracking each walk's origin), walk counts summed level by level in SQL, and, up to 100,000 walks, every walk enumerated in SQL with its path and relations: `paths` without distinct ends must equal that multiset, and each distinct-ends path must be one of its walks. Graphs: int8 with relations, uuid undirected, int4 with tombstones and a tombstoned start, and int8 without a reverse CSR (whose `in` steps and reverse walks are refused). The §9.3 oracles run on 10K-node random graphs; the whole pgrx suite takes about 50 s on PG 16.

`warren.bfs` is retained as the untyped special case (one step, any relation, `max_hops = depth`) with its own simpler signature.

### 9.3 Function reference

All take `graph text` first, external IDs for nodes, and `version bigint DEFAULT NULL`. All are `STABLE`. **Parallel labels (decided: M3, PR #4 review; M5 fix):** every function that writes or may persist what it computes (`create_graph`, `refresh`, `promote`, `compact`, `drop_*`, `grant`, `revoke`, the vector constructors and kernels — `vector_const`, `vector_from_query`, `vector_degree`, `vector_dense_id`, `vector_map`, `spmv`, `spmm`, `gather`, `triangles` — and the library functions) is `PARALLEL UNSAFE`. Every function that reads a graph or a vector is `PARALLEL RESTRICTED`: the traversal functions, `diff`, `info`, `prewarm`, `evict`, `last_stats`, and the vector reads `vector_values`, `matrix_rows`, `id_map`, `vector_reduce`, `vector_norm_diff`, `vector_stats`, `vector_topk`, `explain_map`. A parallel worker has an empty backend-local cache (it would cold-load and hold the whole version), cannot see transient vectors (§6.2), and cannot hand `last_stats()` back to the leader; they return to `PARALLEL SAFE`, where that is true, with the DSM cache (v1.1, §10). Only `version`, `s` and `steps`, which read no graph, are `PARALLEL SAFE`. A restricted function still runs in the leader while a parallel plan executes, which is parallel mode, so the read path assigns no transaction id and writes nothing: its single-row catalog reads go through `read_only::read_one` (a `Spi::connect` select, which never calls pgrx's `mark_mutable` and so never assigns a transaction id; it makes no promise about the snapshot, which is SPI's as for any read), never pgrx's `Spi::get_one*`, which runs the query through `SpiClient::update` and so calls `GetCurrentTransactionId()` ("cannot assign XIDs during a parallel operation"). `parallel_labels`'s tests check every function's label against a declared table and run every non-unsafe function (and reads of a transient vector) under `debug_parallel_query = on`, both in a subtransaction with no transaction id and in a transaction that holds one, and in the InitPlan of a cost-induced parallel scan. Clippy's `disallowed-methods` refuses pgrx's `Spi::get_*`, `run*` and `explain*` outside tests and the writer paths that allow them explicitly.

```sql
warren.bfs(graph text, start anyarray, max_depth int,
           direction text DEFAULT 'out',            -- out | in | both
           rels text[] DEFAULT NULL,
           node_filter anyarray DEFAULT NULL,       -- allowed nodes, external IDs
           max_results bigint DEFAULT NULL, version bigint DEFAULT NULL)
  RETURNS TABLE (node_id <id_type>, depth int, via <id_type>);

warren.neighbours(graph, node, direction DEFAULT 'out', rels DEFAULT NULL) RETURNS SETOF <id_type>;
warren.reachable(graph, start, target, max_depth DEFAULT NULL, rels DEFAULT NULL) RETURNS boolean;
warren.reachable_set(graph, start anyarray, max_depth DEFAULT NULL, rels DEFAULT NULL) RETURNS SETOF <id_type>;
warren.shortest_path(graph, start, target, max_depth DEFAULT NULL, rels DEFAULT NULL) RETURNS <id_type>[];   -- bidirectional BFS
warren.dijkstra(graph, start, target, rels DEFAULT NULL) RETURNS TABLE (path <id_type>[], cost float8);
warren.components(graph) RETURNS TABLE (node_id <id_type>, component_id bigint);
warren.degree(graph, node, direction DEFAULT 'out', rels DEFAULT NULL) RETURNS bigint;
warren.has_edge(graph, src, dst, rel DEFAULT NULL) RETURNS boolean;
warren.walk(graph, start anyarray, steps warren.step[], mode DEFAULT 'ends', ...);   -- §9.2

-- management
warren.create_graph(name, node_query, edge_query, directed DEFAULT true, weighted DEFAULT false,
                    has_relations DEFAULT false, build_reverse DEFAULT true, retain_versions DEFAULT 2);
                    -- owner := current_user; both queries are opened (not run) as read-only cursors to check
                    -- their columns; id_type is the node query's column type: int4, int8 or uuid
                    -- (edge endpoints must have the same type; M3)
warren.refresh(name, promote DEFAULT true, label DEFAULT NULL) RETURNS bigint;   -- the new version_id
warren.promote(name, version); warren.drop_version(name, version); warren.compact(name) RETURNS bigint;
warren.diff(name, v1, v2) RETURNS jsonb;   -- §7
warren.drop_graph(name); warren.prewarm(name); warren.evict(name);
warren.grant(name, role regrole); warren.revoke(name, role regrole);   -- §7 access; both RETURN void
-- promote, drop_version, drop_graph RETURN void; promote applies retention (§7)
warren.info(name) RETURNS jsonb;   -- current version, stats, staleness (now() - built_at), bytes, versions list;
                                   -- owners and readers may call it. Keys: name, owner, readers, id_type, the flags,
                                   -- retain_versions, current_version, staleness, bytes, versions[{version, seq, staged,
                                   -- current, label, built_at, built_lsn, staleness, node_count, id_space, edge_count,
                                   -- dropped_edges, deduplicated_edges, target_width, bytes, compacted_from, vectors}]
warren.last_stats() RETURNS jsonb; -- nodes visited, edges traversed, path taken, for the session's last call (Appendix A.6)
warren.explain_map(graph, expr, version DEFAULT NULL) RETURNS jsonb;  -- 'compiled' | 'executor', and why (§6.3)
```

Typical use:

```sql
SELECT n.* FROM warren.bfs('kg', ARRAY[:root], 6, rels := ARRAY['cites','extends']) r
JOIN entities n ON n.id = r.node_id
WHERE n.kind = 'paper' ORDER BY r.depth;
```

Filtered traversal pushes predicates on intermediate nodes into the engine without a pattern-matching language: `node_filter` is converted to a dense-ID bitmap once per call. Accept a `roaringbitmap` type if that extension is present (v1.1); v1 takes an array.

Implementation notes:
- Traversal is level-synchronous BFS with frontier and next-frontier arrays (parallelisable later).
- Per-call scratch (visited set, depth and predecessor arrays) starts sparse (a hash set or map) and turns dense once it holds more than about `id_space/16` nodes, so a small traversal on a big graph does not allocate O(N) and no visit budget has to be guessed up front. Predecessors are stored only when the signature needs them.
- Truncation is never silent. `truncated = true` is set in `warren.last_stats()`, and a `NOTICE` is raised, exactly when the scan was stopped early by `max_results` or by a GUC ceiling (for example the `paths`-mode cap, §14 item 15). Reaching the caller's own `max_depth` is the requested result, not truncation.
- Interrupts are checked every ~64K units of work (nodes visited plus CSR row entries scanned), so edge scans that admit no node are bounded too. Traversals take an `Interrupts` poll hook: on the main thread (M3) the adapter implements it with `pgrx::check_for_interrupts!()`, which unwinds through the traversal; worker threads (M4) poll a shared `Cancellation` flag the main thread raises.
- There is no OFFSET-style pagination: SRFs stream rows (holding an `Arc` to the cached version) and SQL `LIMIT` stops them. Duplicate removal (`distinct_ends`, multi-start BFS) happens before any cap is applied.
- `shortest_path`'s bidirectional BFS finishes scanning the current level before choosing the meeting node with the minimum combined distance. Taking the first meeting node can return a non-shortest path, a bug pgGraph shipped and fixed. A differential proptest checks it against single-direction BFS.
- `components` labels each component with its minimum dense ID, so results are deterministic and comparable across versions.
- Defaults for caps may come from GUCs inside the SQL `DEFAULT` expression (`current_setting('warren.…', true)`).
- Decisions made in M1 (`warren-core::traversal`, dense ids in and out):
  - `direction` `in` and `both` read the reverse CSR and raise an error when the graph was built without one. `shortest_path` without a reverse CSR runs a forward-only BFS instead; `components` never needs one.
  - `bfs` and `reachable_set` also come in streaming forms (`bfs_each`, `reachable_set_each`) that hand each row to a visitor, so an SRF under SQL `LIMIT` stops the scan by breaking. A visitor stopping the scan is not truncation.
  - `neighbours` and `degree` count each neighbour once: `Adjacency::neighbours` returns the distinct dense IDs across the selected relations (and both directions for `both`) in ascending order, by a k-way merge of the sorted rows.
  - `node_filter` applies to the nodes a traversal reaches, never to its starts: starts are always admitted at depth 0, and repeated starts count once. `reachable_set` includes the starts.
  - `last_stats()` counts `nodes_visited` (nodes admitted or settled, starts included) and `edges_traversed` (CSR row entries scanned). The `Interrupts` hook is polled on the first unit of work (a visited node or a scanned row entry) and every 65,536th after.
  - `shortest_path` breaks ties between meeting nodes of equal combined distance by the smallest dense ID, so its result is deterministic.
  - `dijkstra` reads each selected relation's own CSR rather than the merged one, because the merged CSR sums the weights of an edge present in several relations and a cheapest path wants the minimum. Unweighted graphs cost 1 per edge. A negative or NaN weight raises an error when the search scans it. An edge whose cost would be infinite (an infinite weight, or an overflowing sum) is never followed, so a target reachable only at infinite cost returns no path.
  - `components` is union–find over the forward all-relations CSR. The domain labels every dense ID; a tombstoned node has no edges and so is its own component, and the adapter leaves tombstoned nodes out of the result.
- Decisions made in M3 (`pg_warren::traversal_sql`, external ids in and out):
  - **Signatures as implemented.** `bfs(graph, start anyarray, max_depth int, direction DEFAULT 'out', rels DEFAULT NULL, node_filter "any" DEFAULT NULL, max_results DEFAULT NULL, version DEFAULT NULL) RETURNS TABLE (node_id anyelement, depth int, via anyelement)`; `reachable_set(graph, start anyarray, max_depth DEFAULT NULL, rels DEFAULT NULL, direction DEFAULT 'out', version DEFAULT NULL) RETURNS SETOF anyelement`; `neighbours(graph, node anyelement, direction DEFAULT 'out', rels DEFAULT NULL, version DEFAULT NULL) RETURNS SETOF anyelement`; `degree(…same…) RETURNS bigint`; `has_edge(graph, src anyelement, dst anyelement, rel text DEFAULT NULL, version DEFAULT NULL) RETURNS boolean`; `reachable(graph, start anyelement, target anyelement, max_depth DEFAULT NULL, rels DEFAULT NULL, direction DEFAULT 'out', version DEFAULT NULL) RETURNS boolean`; `shortest_path(…same…) RETURNS anyarray`; `dijkstra(graph, start, target, rels DEFAULT NULL, direction DEFAULT 'out', version DEFAULT NULL) RETURNS TABLE (path anyarray, cost float8)`; `components(graph, id_type anyelement, version DEFAULT NULL) RETURNS TABLE (node_id anyelement, component_id bigint)`; `last_stats() RETURNS jsonb`. `direction` was added (after `rels`) to `reachable`, `reachable_set`, `shortest_path` and `dijkstra`, since the core supports it and `in` is otherwise inexpressible.
  - **Polymorphism (§14 item 1): `anyelement`/`anyarray`, one C function each, dispatching at run time on `graphs.id_type`.** An id argument must have exactly the graph's id type (`datatype_mismatch` otherwise, with the cast to use): Postgres resolves the result type from the argument, so an `int4` literal against an `int8` graph cannot return `int8` ids. Integer literals therefore need a cast on `int8` graphs (`ARRAY[1::bigint]`, `42::bigint`). Lookups whose result holds no ids (`degree`, `has_edge`, `reachable`) widen an `int2`/`int4` argument losslessly to the graph's integer id type, so `warren.degree('g', 42)` works on a `bigint` graph. An array argument that is not an array (`node_filter`, `warren.s`'s `filter`, including an untyped literal like `'{1,2}'`) is `datatype_mismatch` naming the array cast to write.
  - Postgres coerces a polymorphic parameter's default to the pseudo-type itself, so an omitted `anyarray DEFAULT NULL` breaks type resolution. `node_filter` is therefore declared `"any"` (C functions only), defaults to `NULL`, and is checked at run time to be an array of the id type. For the same reason `components`, which has no id argument, takes a type witness, as `json_populate_record` does: `warren.components('g', NULL::bigint)`.
  - **Unknown ids.** An external id the version does not have, or has only as tombstoned, is not a node of that version. As a start or `node_filter` member it is skipped (so is NULL); as the node of a lookup it finds nothing: `neighbours` is empty, `degree` 0, `has_edge` and `reachable` false (an unknown node does not even reach itself), `shortest_path` NULL, `dijkstra` no row. A NULL node argument gives NULL (no rows). Relation names a version does not have are skipped likewise (a relation absent from a version has no edges in it); `rels => '{}'` selects nothing. Unknown ids are not errors because a version is a snapshot: ids come and go between versions, and the SQL side of the seam (§9.1) treats a missing join partner the same way.
  - **Streaming.** `bfs` and `reachable_set` run over `warren_core::traversal::BfsCursor`, which owns an `Arc` of the cached version and runs one BFS level per call; the SRF returns a level's rows before running the next. Postgres fetches value-per-call SRFs one row at a time only in the select list (`SELECT warren.reachable_set(…) LIMIT 10`); in `FROM` it runs the function to completion into a tuplestore, so there `LIMIT` does not shorten the scan and `max_results` is the bound to use. `last_stats()` is updated after every level, so after a `LIMIT` it counts the work actually done.
  - `last_stats()` is a session's `jsonb` `{function, graph, version, nodes_visited, edges_traversed, truncated}` (NULL before the first recording call); it is `VOLATILE PARALLEL RESTRICTED`. The `NOTICE` on truncation is raised once per call. `components` reports the union–find work.
  - Errors: a missing reverse CSR is `object_not_in_prerequisite_state`; a negative or NaN weight is `data_exception`; a negative `max_depth`/`max_results` or an unknown direction is `invalid_parameter_value`; a NULL `graph` or `direction` is `null_value_not_allowed`.
  - Cancellation: the `Interrupts` poll is `pgrx::check_for_interrupts!()` on the main thread, so `statement_timeout` and `pg_cancel_backend` stop a long traversal with `query_canceled` within 64K units of work.

## 10. Future directions (not v1)

- **Incremental maintenance.** Triggers or a logical-replication consumer write deltas to `warren.deltas`; traversal consults CSR + a small overlay (added edges in a hash map, removed in a tombstone set); a background worker compacts into a new version when the overlay grows. Seconds-level staleness without full rebuilds. Note that the stable-ID design already supports this: a delta-compacted version is just another version in the same ID space.
- **DSM shared cache and compute worker** (§6.9). Probably the first post-v1 item: it removes the per-backend load cost and lets compute run detached from a client session. pgGraph's Linux-only sealed-`memfd` sharing of an immutable copy is an alternative worth measuring against `dsa`.
- **Direction-optimising BFS** (push/pull switch when the frontier exceeds ~N/20 and the reverse CSR exists) for `reachable_set` and `components`. This is how GraphBLAS reaches multi-billion edges/s.
- **Parallel traversal** via the same thread pool as compute.
- **WASM vertex programs** (§6.8).
- **Text IDs**; **edge properties** beyond one weight (parallel arrays); **node property vectors** auto-synced from table columns.
- **Arnoldi** for non-symmetric spectra; **randomised SVD**; **spectral clustering** as a library algorithm (eigen → k-means over rows).
- **Pattern sugar, if ever.** `warren.walk` covers typed chains. What a pattern *language* would add, in ascending order of value: joins within a pattern (cycles, shared neighbours) and variable binding of intermediate nodes — both expressible today by intersecting walks in SQL — and **selectivity-aware start selection**, which is the real reason graph query planners exist and which `reverse` only approximates. If a language is ever added it should be SQL/PGQ (`GRAPH_TABLE`, SQL:2023) rather than Cypher: it is the standard, composes with SQL rather than sitting beside it, and Postgres has no implementation yet. The shape would be `warren.match('MATCH (a:paper)-[:cites*1..3]->(b)-[:authored_by]->(p) ...')` parsing to `warren.step[]` and a small join plan over walks — sugar over the existing IR, not a second engine. Not before v1 has demonstrated that users hit the limits of `walk`.
- **Custom scan node** so joins with traversals get cardinality estimates from version statistics.
- **GPU backend** for `warren-core` kernels.

## 11. Implementation language and tooling

**Rust via [pgrx](https://github.com/pgcentralfoundation/pgrx).** The hot code is array manipulation and numerical kernels; pgrx gives SPI, SRFs, memory-context integration, GUCs, and a test harness that runs against real Postgres instances across versions. Target Postgres 16+ as baseline; test on 15–18. No runtime dependencies beyond Postgres.

Toolchain discipline (adopted from pgGraph's working setup):
- `pgrx` and `cargo-pgrx` are pinned to the same exact version (`=0.19.3`) and upgraded together in their own change. The Rust toolchain is pinned in `rust-toolchain.toml`.
- Per-major features are `pg15`…`pg18`, built with `--no-default-features --features pgNN`. `panic = "unwind"` is set in every profile.
- Lints: `unsafe_op_in_unsafe_fn = "deny"` and `clippy::undocumented_unsafe_blocks = "deny"`. `unwrap`/`expect`/`panic` are denied outside `cfg(test)`. Every `unsafe` block carries a `// SAFETY:` proof, and `warren-core`'s unsafe slice code runs under Miri in CI.

Workspace split, non-negotiable:

- `warren-core` — CSR, traversal, semiring kernels, gather, Lanczos. **No Postgres dependency.** Takes slices and a thread count; returns results. Unit-, property- and bench-tested standalone. `rayon` for parallelism; `faer` or `nalgebra` for small dense problems; `proptest` and `criterion` in dev-deps.
- `pg_warren` — the pgrx extension: catalog, SPI build pipeline, chunked persistence, cache, SQL bindings, GUCs. Thin.

## 12. Testing and benchmarking

### Correctness

- `warren-core` unit and property tests: CSR construction (duplicates, self-loops, dangling edges, tombstones, stable-ID merge across builds), BFS, shortest paths, components, every semiring kernel against a naive dense reference on small random graphs, `gather` with fixed- and variable-width messages against a naive implementation, Lanczos against a dense eigensolver on graphs with N ≤ 500.
- `vector_map`: every expression in Appendix A and the library takes the compiled path (asserted via `warren.explain_map`); compiled and executor paths agree bit-for-bit on random expressions over random vectors.
- **Use-case tests (Appendix A):** each worked example is an integration test with a generated fixture and the stated expected outcome. *Implemented (M2):* `bench/generate.py` writes seeded, deterministic fixtures (CSV + `load.sql` + ground truth) for every Appendix A fixture and the benchmark graphs; `test/run.py` runs `test/sql/*.sql` in fresh databases of a scratch cluster, with `-- fixture:` / `-- oracle:` directives and `assert.*` functions that raise on failure (format in `test/README.md`).
- Integration tests (`cargo pgrx test`): every SQL function; every `id_type`; directed/undirected; weighted/unweighted; relations present/absent; reverse present/absent; filters; refresh with a prior version loaded; staged → promote → retention → drop; vectors bound to retained versions; `compact` and the refusal to compare across it; rollback of a transaction containing a staged build leaves no rows; cancellation of long traversals and compute; memory cap enforcement.
- **Oracle tests:** every traversal function against an equivalent recursive CTE; PageRank and components against NetworkX on exported fixtures; `gather(... 'collect')` against a plain SQL join + `array_agg`; `diff` against SQL `EXCEPT`. Exact match on random graphs of ~10K nodes. This suite is the most valuable thing in the repository.
- Robustness: a `cargo fuzz` target for the segment decoder (§5.3); a differential proptest of bidirectional against single-direction shortest path (§9.3).
- Extension lifecycle: `cargo pgrx schema` output diffed against the committed `sql/pg_warren--X.Y.Z.sql` (*M2:* `sql/schema-drift.sh`, in CI on PG 16; both sides are normalised — comments dropped, entity blocks sorted — because pgrx's order is unstable and its comments carry source lines). From the first release on, install the previous release, run `ALTER EXTENSION pg_warren UPDATE`, and assert the resulting `pg_proc`/`pg_class` signatures equal a fresh install's (*implemented after 0.1.0:* `test/upgrade.py`, in CI on PG 15–18, compares functions with their signatures, defaults and attributes, relations, columns, storage, constraints, indexes, sequences, types, casts, grants, comments and extension configuration, and checks backfilled rows and a retention pass over versions written before the update; released install scripts are frozen by `sql/released.sha256`). A `pg_dump`/restore round trip preserves `warren.*` rows (§5.1).
- Cross-version tests: build v1, mutate tables (add/remove nodes and edges), build v2, assert dense IDs of surviving nodes unchanged, assert `distance` is zero when nothing changed and monotone in perturbation size for `edit`.

### Performance

Targets on a commodity 8-core machine, graph in cache, 10M nodes / 100M edges, avg degree 10:

| Workload | Target |
|---|---|
| BFS to depth 6 from one node | ≥ 20× faster than recursive CTE with btree on `(src)` |
| `reachable_set`, full component | > 50M edges/s traversed |
| `components()` | < 10 s |
| `pagerank`, 20 iterations, 8 threads | < 30 s (≥ 5 GE/s SpMV per thread-group) |
| `aggregate_neighbours`, d = 768, f32 | < 5 min |
| `eigen`, k = 20, normalised Laplacian | < 10 min |
| `refresh()` incl. reverse CSR | < 8 min, < 4 GB RAM |
| `diff` between two versions | < 30 s |
| Cold load into backend | < 15 s |

Benchmark harness: generators for random, power-law (preferential attachment), grid, and *evolving* graphs (a base graph plus a parameterised perturbation, so `distance` methods can be evaluated for sensitivity and monotonicity); loads into `nodes`/`edges`; builds; runs the matrix against baselines (recursive CTE, NetworkX, igraph). Commit results per version to `bench/results/`. *Implemented (M2):* `bench/run.sh` runs `bench/matrix.json`; a workload names the functions it `requires` and is recorded as skipped until they exist. SNAP datasets (LiveJournal, Orkut, road networks) as secondary benchmarks.

## 13. Repository layout and milestones

```
pg_warren/
├── README.md               ← user-facing: what it is, install, 10-line example, semantics
├── DESIGN.md               ← this document, kept current
├── Cargo.toml              ← workspace
├── crates/
│   ├── warren-core/        ← CSR, traversal, kernels, gather, eigen; no Postgres dependency
│   └── pg_warren/          ← pgrx extension
├── docs/                   ← prior-art-notes.md (§4A), semantics, limits
├── sql/                    ← hand-written SQL, upgrade scripts
├── bench/                  ← generate.py, run.sh, results/
├── test/                   ← integration tests not expressible in cargo pgrx test
├── docker/                 ← one Dockerfile, parameterised by a PG_MAJOR build arg
└── .github/workflows/      ← fmt, clippy, unit, Miri (warren-core), pgrx tests PG 15–18 on PGDG packages
                               (cargo pgrx init --pgNN=<pg_config>), schema-drift check (added in M2); upgrade-path check from the first release (v0.1.0) on (`test/upgrade.py`, added for 0.2.0); bench targets compile+run once in core tests
```

Each milestone leaves `main` green and demonstrable.

**M0 — Skeleton and prior-art reading.** Layout, pgrx builds, `CREATE EXTENSION pg_warren`, `warren.version()`, CI. Licence: Apache-2.0. The §4A reading is completed, `docs/prior-art-notes.md` is written, and §14 item 16 is recorded before M1 begins.

**M1 — warren-core: structure and traversal.** CSR with per-relation segments and tombstones; stable-ID merge; BFS, reachable set, shortest path, components. Property tests. `criterion` micro-benchmarks.

**M2 — Catalog, versions, build pipeline.** `create_graph`, `refresh` (promote/staged), `promote`, `drop_version`, retention, `compact`, `info`, `diff`. Chunked persistence. `int8` IDs. Tests for rollback-cleans-up and stable IDs across builds.

**M3 — Cache and traversal SQL.** Loader, all §9.3 traversal functions, `prewarm`/`evict`, oracle tests vs recursive CTE. `int4`, `uuid`. Relations and filters. `warren.walk` with `ends`/`count` modes, variable-length segments and `reverse`; oracle tests vs an equivalent chain of CTEs. `paths` mode may slip to M7 if predecessor bookkeeping proves intricate. Tag `v0.1.0` — a usable traversal index.

**M4 — Vectors and kernels.** `warren.vectors` persistence; `vector_from_query`, `matrix_rows`, `id_map`, etc.; `Message`-generic buffers; semiring SpMV/SpMM, `vector_map` with SQL-expression parsing (compiled subset + executor fallback + `explain_map`), `vector_reduce` in `warren-core` with `rayon`; GUCs for threads and memory cap; cancellation. *Done:* kernels in `warren-core::compute`; persistence and cache in `pg_warren::vector`; the SQL surface in `pg_warren::compute_sql` (§6.2, §6.3), with oracle tests against SQL joins and aggregates. `compact` carries persisted vectors into the new id space (§5.2).

**M5 — Gather and algorithm library.** `warren.gather` with all combiners; the PL/pgSQL library (PageRank, components-via-LPA, label propagation, SSSP vector, degree, `aggregate_neighbours`, triangles). Oracle tests vs NetworkX and vs SQL joins. Appendix A.1 (fraud ring) and A.3 (entity resolution) as integration tests. Benchmarks for the compute rows of §12.

**M6 — Spectral and distance.** Lanczos `eigen` with f32/f64 modes; `distance` with all five methods; evolving-graph benchmark; the §8 workflow and Appendix A.2 as integration tests end-to-end. Tag `v0.2.0`.

**M7 — Hardening.** A size-bounded LRU for the backend-local cache (`warren.max_cache_memory`, §5.5); 100M-edge soak; memory-context and allocator hygiene; docs for semantics, staleness, retention, the two change-detection patterns, and limits. Decide §6.10 from benchmarks. Tag `v0.3.0`.

Post-v0.3: contraction/project/lift and Louvain (§6.6), DSM cache + compute worker, incremental maintenance, WASM programs, in roughly that order.

## 14. Open decisions (make a call, record it in DESIGN.md, move on)

1. **Polymorphism over `id_type`** in SQL: `anyelement` runtime dispatch vs. generated overloads. Lean: `anyelement`. **Decided (M3): `anyelement`/`anyarray`** with run-time dispatch on `graphs.id_type` and an exact type match for id arguments; `"any"` for `node_filter` and a type witness for `components` (§9.3).
2. **Persistent store**: chunked `bytea` only, or additionally a regenerable `$PGDATA/pg_warren/*.csr` file for mmap? Lean: `bytea` only; measure before adding. Evidence from pgGraph (`docs/prior-art-notes.md` §1.3–1.4) supports this. Its file artifacts are copied and fully validated on every load anyway, so they are not zero-copy. They needed MVCC catalog publication, per-database namespacing and interrupted-replacement recovery added after the fact, and they have no standby support. M2 measures cold load from `warren.segments`; revisit only if it falls below ~300 MB/s.
3. **Chunk size** (4/8/32 MB). Measure.
4. **Parallel-edge deduplication**: drop with count; if weighted, sum or keep-first? **Decided (M1): sum**, in input order, counted in `versions.deduplicated_edges`; unweighted duplicates are dropped with the same count. pgGraph keeps parallel edges only because it returns per-edge source-row identities, which pg_warren does not.
5. **Self-loops**: keep. **Decided (M1): kept**; an undirected self-loop is stored once.
6. **Undirected weighted**: duplicate weight both directions. **Decided (M1): yes**, both directions carry the same weight.
7. **Default `max_depth`** on `reachable_set`: unbounded vs. cap. Lean: unbounded, documented. **Decided (M3): unbounded** (`max_depth DEFAULT NULL`); the visited set bounds the work at O(N + M), and SQL `LIMIT` in the select list stops it early (§9.3).
8. **Lanczos precision default**: f32 vectors/f64 accumulate vs. all f64. **Decided (M6): f32 by default**, every sum accumulated in f64, `Precision::F64` as the option; residuals floor near 10⁻⁷·‖M‖ in f32 (§6.5). On the 100K-node baseline f32 and f64 take the same steps (1,037 and 1,342) and their step times are within the noise; f32 halves the basis memory, which is what binds at 10⁷ nodes.
9. **Native kernels vs. SuiteSparse:GraphBLAS** (§6.10). Lean: native for v1; revisit after M6. M4 implements the kernels natively in `warren-core` (§6.3).
10. **Compute threads inside the backend vs. requiring the background worker from day one.** Lean: in-backend threads for v1 (simpler lifecycle), worker in v1.1. M4 implements in-backend threads: a dedicated `rayon` pool per `compute::Workers` (§6.9). **Decided (M4):** in-backend, one pool per backend sized by `warren.threads`, its threads blocking every signal.
11. **`distance` summary statistics**: which scalar(s) each method returns by default. **Decided (M6): several per method**, returned as `jsonb` (§8): `edit` counts and Jaccard distance per relation and overall; `degree` W₁ and KS; `spectral` L2, relative and max; `subspace` projection-Frobenius distance, max angle and the angles; `embedding` mean/max L2 and cosine and the rows compared.
12. **`vector_map` executor fallback.** Build it in v1 or raise "not in compiled subset" until v1.1? Lean: build it in v1 — the one-language guarantee is the point. **Decided (M4): built** — row-wise `ExecEvalExpr` over the same analysed tree, JIT-compiled per the `jit` settings, agreeing with the compiled path bit for bit (§6.3).
13. **`gather` message source.** Accept only a SQL string, or also a relation name / vector name? Lean: all three, dispatched on the argument's form. **Decided (M5): all three** — a bound vector's name, else an identifier as a relation, else a query (§6.4); the domain kernel takes `(dense id, message)` pairs whatever the source.
14. **`warren.step` as a composite type vs. `jsonb`.** Composite gives type-checking and a clean `warren.s()` constructor; `jsonb` is easier to generate from application code. Lean: composite as the canonical IR with a `jsonb → warren.step[]` cast for convenience.
15. **`walk` path enumeration limits.** Hard cap on `max_results` for `mode = 'paths'` (lean: default 100,000, error above a GUC ceiling) and whether to support `'count'` exactly or approximately for variable-length segments (lean: exact). **Decided (M3, domain):** `count` is exact, for distinct ends and for walks (`u64`, an error on overflow); see §9.2. The domain bounds `paths` only by the `max_results` it is given; the default and ceiling are the adapter's (lean stands). **Decided (M3, adapter):** default 100,000 rows for any walk reporting a row per walk (`paths`, or `ends` without `distinct_ends`), and a `max_results` above the `warren.max_walk_paths` GUC (default 10,000,000) is refused (§9.2).
16. **Prior-art reading complete** (§4A). **Done (M0, 2026-10-19):** see [`docs/prior-art-notes.md`](docs/prior-art-notes.md) (pgGraph `b0f53df`, OneSparse `79a57cd`). Sections changed as a result (v0.6): §4A, §5.1, §5.3, §5.4, §5.5, §6.3, §6.7, §6.9, §7, §9.3, §10, §11, §12, §13, §14 items 2 and 4. Not a reuse decision (the policy is to build in full), but a gate.
17. **Relation count × node count.** Per-relation offsets grow as R·N (§5.3). **Decided (M1):** no fixed relation limit; a build is refused up front when its offsets would exceed the memory budget the adapter passes. **Decided (M2):** the budget is the `warren.max_build_memory` GUC (kB units, default `4GB`, minimum `1MB`, `USERSET` like `maintenance_work_mem`); `refresh` passes it to `GraphCsr::build_within`, and an over-budget build fails with `configuration_limit_exceeded` before anything is allocated or written. It bounds the offsets (the R·N term) only; the edge list and targets are bounded by fallible allocation. Revisit with sparse per-relation offsets if graphs with many relations appear.
18. **Transient vectors.** Persisting every vector write costs WAL and TOAST per superstep (≈320 MB per f64 vector at 10M nodes) and pins old versions against retention. **Decided (M4 review):** per-call `persist bool DEFAULT true`; `persist := false` keeps the vector only in the backend, non-transactionally, without pinning retention (§6.2). Rejected: `_`-prefixed names (magic), and an `UNLOGGED` scratch table (still TOAST and vacuum cost, still per-superstep writes). Revisit with M7's cache bound (transient and cached vectors count against the backend's LRU then).

## 15. Instructions to the implementing agent

1. Read this document in full, including Appendix A — the use cases there are the acceptance criteria. Then read the pgrx documentation on SPI, set-returning functions, memory contexts, GUCs, and background workers.
2. Create the repository layout in §13 and complete M0, including the §4A prior-art reading. Commit. Do not start M1 until `docs/prior-art-notes.md` and §14 item 16 exist. Everything in `pg_warren` is written in this repository; prior art is a source of lessons, never a dependency.
3. Work through milestones in order. Do not start M3 before M2's rollback and stable-ID tests exist; do not start M5 before M4's kernels have property tests against a dense reference.
4. `warren-core` must never import pgrx or anything Postgres-specific. CI should fail if it does.
5. Keep `DESIGN.md` updated whenever a §14 decision is made or a design detail changes. The document should always describe the code as it is.
6. Prefer boring, measurable progress. Between "clever and fast" and "simple and correct", pick simple and correct, benchmark, then optimise what the benchmark says to optimise.
7. Record unresolved questions as GitHub issues with a proposed default, and take the default unless overruled.

---

## Appendix A — Worked use cases, expected outcomes, and the tests they imply

Each example below is a concrete program a user is expected to write against the v1 API. For the implementing agent, each one is also a specification: the **fixture** describes a small synthetic dataset to generate, the **expected outcome** is what the test asserts, and the **scale target** is the benchmark to record. Where a snippet uses an API that is v1.1 (`contract`, `project`, `lift`), the test is written against the v1 stage-1 form and the v1.1 form is added when it lands.

### A.1 Fraud ring detection (tier 1, then contraction)

**Scenario.** A payments platform holds accounts, IP addresses, device fingerprints and email addresses in four tables, with usage events linking accounts to identifiers. Fraud rings are groups of accounts that share *several* identifiers; a shared NAT IP or office device is noise. Some accounts are already flagged (chargebacks, confirmed fraud). The goal is to rank rings by suspicion and surface unflagged accounts in them.

**Graph.**

```sql
CREATE VIEW fraud_nodes AS
  SELECT id, 'account' AS kind FROM accounts
  UNION ALL SELECT id, 'ip'     FROM ip_addresses
  UNION ALL SELECT id, 'device' FROM device_fingerprints
  UNION ALL SELECT id, 'email'  FROM email_addresses;

SELECT warren.create_graph('fraud',
  node_query := 'SELECT id FROM fraud_nodes',
  edge_query := $$
     SELECT account_id, ip_id,     'used_ip'     FROM account_ip_events
     UNION ALL SELECT account_id, device_id, 'used_device' FROM account_device_events
     UNION ALL SELECT account_id, email_id,  'has_email'   FROM account_emails $$,
  directed := false, has_relations := true);
SELECT warren.refresh('fraud');
```

**Program.** Seeded, degree-normalised suspicion propagation with hub masking — personalised PageRank over the identifier graph, written as PL/pgSQL over tier-1 primitives.

```sql
CREATE FUNCTION fraud_suspicion(graph text, hub_degree int DEFAULT 50, damping float8 DEFAULT 0.7)
RETURNS int LANGUAGE plpgsql AS $$
DECLARE ran int := 0; delta float8; mass float8;
BEGIN
  PERFORM warren.vector_from_query(graph, 'seed',       'SELECT id, 1.0 FROM accounts WHERE confirmed_fraud OR chargebacks >= 2');
  mass := warren.vector_reduce(graph, 'seed', 'sum');
  PERFORM warren.vector_from_query(graph, 'is_account', 'SELECT id, 1.0 FROM accounts');
  PERFORM warren.vector_degree(graph, 'deg');
  PERFORM warren.vector_map(graph, 'live',    format('CASE WHEN is_account = 1 OR deg <= %s THEN 1 ELSE 0 END', hub_degree));
  PERFORM warren.vector_map(graph, 'inv_deg', 'CASE WHEN deg > 0 THEN 1.0 / deg ELSE 0 END');
  PERFORM warren.vector_map(graph, 'score',   'seed');
  FOR i IN 1..50 LOOP
    ran := i;   -- the FOR variable is not visible after the loop
    PERFORM warren.vector_map(graph, 'msg',      'score * inv_deg * live');
    PERFORM warren.spmv(graph, 'msg', 'gathered', semiring := 'plus_times', mask := 'live');
    PERFORM warren.vector_map(graph, 'next',     format('%s * seed + %s * gathered', 1 - damping, damping));
    delta := warren.vector_norm_diff(graph, 'next', 'score', 'l1');
    PERFORM warren.vector_map(graph, 'score',    'next');
    EXIT WHEN delta < 1e-6 * mass;   -- relative to the seed mass
  END LOOP;
  RETURN ran;
END $$;
```

Then cluster the suspicious subgraph with min-label propagation on the masked graph (`warren.label_propagation` restricted by a `score > 0.01 AND live = 1` mask, seeded from `warren.vector_dense_id`), and aggregate in SQL:

```sql
WITH members AS (
  SELECT l.node_id, l.value::bigint AS ring, s.value AS score, n.kind
  FROM warren.vector_values('fraud', 'label', NULL::bigint) l
  JOIN warren.vector_values('fraud', 'score', NULL::bigint) s USING (node_id)
  JOIN fraud_nodes n ON n.id = l.node_id
  WHERE s.value > 0.01)
SELECT ring,
       count(*) FILTER (WHERE kind = 'account')  AS accounts,
       count(*) FILTER (WHERE kind <> 'account') AS identifiers,
       sum(score)                                AS total_suspicion,
       array_agg(node_id) FILTER (WHERE kind = 'account' AND score < 1) AS unflagged_members
FROM members GROUP BY ring
HAVING count(*) FILTER (WHERE kind = 'account') BETWEEN 3 AND 500
ORDER BY total_suspicion DESC LIMIT 50;
```

v1.1 extension: `warren.contract('fraud', 'label', 'fraud_rings')` gives a ring-level graph on which `project` yields per-ring suspicion mass, account counts and identifier ratios as vectors, and `distance` between two versions answers "is a new ring forming between existing ones?".

**Fixture.** Generator parameters: 100,000 honest accounts, each with 1–3 own identifiers (unique IPs/devices/emails); 20 planted rings of 5–40 accounts, each ring sharing 2–6 identifiers among its members; 3 hub identifiers (a NAT IP shared by 5,000 accounts, a shared device by 800, a disposable-email domain node by 2,000) attached to a random mix of honest and ring accounts; 30% of ring accounts and 0.5% of honest accounts are seeded as flagged.

**Expected outcome (assertions).**
- Every planted ring with a flagged account appears among the top 25 rings by `total_suspicion` (a ring with no flagged account sends no suspicion, so seeded propagation cannot find it); no ring with more than 500 accounts exists in the output (hubs did not merge the population).
- ≥ 90% of unflagged ring members have `score > 0.01`; ≤ 1% of honest accounts do.
- Removing the hub mask (`hub_degree := 1e9`) makes output rings mix planted rings and the largest grow at least 5× — the test that the masking is doing the work. (Rings of thousands of accounts do not form: a hub passes on its score divided by its degree, below the 0.01 cut, so a hub joins only the suspicious accounts it links.)
- `fraud_suspicion` converges within its 50-iteration cap: the accounts–identifiers graph is bipartite, so the L1 change contracts by exactly `damping` (0.7) per step, and the relative tolerance 1e-6 takes about 40 iterations (an absolute 1e-6 on ~700 units of seed mass would take ~57); every `vector_map` expression in it takes the compiled path.

*Measured (M5, `test/sql/a1_fraud_rings.sql`, at the fixture scale above):* the program converges in about 40 iterations; no output ring exceeds 500 accounts; 98% of unflagged ring members and 0.5% of honest accounts (the flagged ones) score above 0.01; one 6-account planted ring has no flagged account and is not found; without the hub mask output rings mix planted rings and the largest grows from 39 to 340 accounts.

**Scale target.** 10M accounts, 100M edges, 8 threads: suspicion program < 60 s end-to-end; label propagation < 30 s; aggregation query < 30 s.

### A.2 Corpus ingest gated by structural change (versions, staging, spectral distance)

**Scenario.** A knowledge system holds entities and typed relations extracted from documents, with pgvector embeddings on entities. A new corpus is extracted into the same tables. Before committing, the operator wants to know whether the new material reshapes the graph's global structure (new dominant clusters, merged communities) as opposed to merely adding volume, and to gate the commit on that.

**Program.** The §8 workflow verbatim: ingest inside a transaction, `refresh(promote := false)`, `diff`, `eigen` on both versions, `distance('subspace')`, `distance('embedding')` on neighbour-aggregated embeddings, then `COMMIT` or `ROLLBACK`.

**Fixture.** Base graph: 50,000 entities in 40 planted communities (stochastic block model, p_in = 0.05, p_out = 0.0005), with 64-dim embeddings drawn per community around a community centroid. Three ingest variants, each adding 5,000 entities and ~25,000 edges:
- *volume*: new entities join existing communities with the same block parameters;
- *bridge*: new entities connect two existing communities densely enough to merge them;
- *novel*: new entities form a new community of their own.

**Expected outcome (assertions).**
- `diff` reports edge counts within 5% of the generator's for all three variants (it measures volume and cannot distinguish them — documented). The graph is undirected, and `diff` counts each undirected edge once per direction, so the comparison is against twice the generator's count.
- `distance('subspace', k := 40)`: *volume* < *novel* < *bridge*, with *volume* below a documented threshold and *bridge* above it; results are identical across two runs (determinism of Lanczos with fixed seed).
- `distance('embedding')` after `aggregate_neighbours` ranks *bridge* highest (the merged communities' embeddings drift towards each other). At reduced scale (`test/sql/a2_spectral.sql`) it does, by a quarter over *volume* (§8).
- After `ROLLBACK`, `warren.versions`, `warren.segments` and `warren.vectors` contain no rows for the staged version; after `COMMIT` without `promote`, the staged version is queryable by explicit `version` and invisible by default; dense IDs of all pre-existing entities are identical in both versions.
- The same comparison run via the shadow-schema pattern (second graph, cross-graph `distance` aligned by external ID) gives the same scalars to within 1e-9.

**Scale target.** 10M entities, 100M edges, k = 20: staged refresh < 8 min, both `eigen` calls < 10 min each, `distance('subspace')` < 30 s.

### A.3 Entity resolution by neighbourhood accumulation (tier 2)

**Scenario.** Customer records from several sources share identifiers (phone, email, postal address, payment card last-four) with varying coverage. The goal is to accumulate, per record, the set of identifiers reachable within a bounded neighbourhood and the set of records they imply, with per-record SQL logic (normalisation, source-specific trust rules) that is not arithmetic.

**Program.** Tier 2: state in a table, `gather(... combine := 'collect')` per superstep, `UPDATE` with array/jsonb logic in SQL, bounded to 3 supersteps so neighbourhoods stay local. A final `components_vector` over the "same entity" edges derived from the accumulated sets produces entity IDs.

**Fixture.** 200,000 records across 3 sources; 60,000 underlying true entities, each with 1–5 records; identifier coverage 40–90% per source; 2% of identifiers deliberately shared across unrelated entities (a shared office phone) to create false links that the SQL trust rules must down-weight.

**Expected outcome (assertions).**
- `gather('collect')` output equals the result of the equivalent SQL join + `array_agg` on the same message relation, for every superstep (oracle).
- Variable-width messages (text[] of identifiers) round-trip without truncation or reordering within a node's collected array beyond what `collect` documents (ascending sender dense id, then message order, §6.4).
- Pairwise precision ≥ 0.98 and recall ≥ 0.95 against the generator's ground truth after the trust rules; without them, precision drops below 0.9 (the test that tier-2 SQL logic is load-bearing).
- Memory accounted to the compute pool stays under `warren.max_compute_memory` and an attempt with the GUC set to 64 MB fails with the documented error rather than OOMing.

*Measured (M5, `test/sql/a3_entity_resolution.sql`, at a tenth of the fixture: 20,000 records of 6,000 entities):* three `collect` supersteps over the record–identifier graph — identifiers send `kind:value` to their records; records send their ids to their identifiers; identifiers pass every holder's id on, one message each — each equal to the SQL join + `array_agg` in collect order. The trust rule: two records conflict if they carry identifiers of some kind but none in common; an identifier is untrusted if any two of its holders conflict; a link is kept if its records share a trusted identifier. `components_vector` over the kept links then gives pairwise precision 0.988 and recall 0.955; keeping every link gives precision 0.80. With `warren.max_compute_memory` at its 64 kB minimum (the scale-adjusted form of the 64 MB attempt) a superstep fails with `configuration_limit_exceeded` (53400) before allocating. (The trust SQL needs `ANALYZE` on its intermediate tables: without statistics the planner's join plan exhausted the disk.)

**Scale target.** 10M records, 50M edges: each superstep < 20 s; three supersteps plus components < 2 min.

### A.4 Typed multi-hop retrieval for a research knowledge graph (`walk`)

**Scenario.** Papers cite papers; papers are authored by people; people are affiliated with institutions. A retrieval step wants: UK institutions reachable from a root paper via 1–3 citation hops, then authorship, then affiliation, with only currently active people counted.

**Program.** The §9.2 example: a three-step `warren.walk` with a variable-length first segment and filters on steps two and three, joined back to `institutions`.

**Fixture.** 100,000 papers in a citation DAG with power-law in-degree; 30,000 people with 1–6 papers each, 70% active; 2,000 institutions, 15% in GB; a planted root paper whose 3-hop citation neighbourhood contains ~400 papers.

**Expected outcome (assertions).**
- `walk(... mode := 'ends')` returns exactly the set produced by the equivalent chain of three CTEs (oracle), for `reverse := false` and `reverse := true`.
- `mode := 'count'` equals the cardinality of `ends`; `mode := 'paths'` with `max_results := 1000` returns ≤ 1000 rows each of whose paths validates against the edge tables and whose intermediate nodes satisfy the filters.
- Dropping the step-two filter changes the result set (filters prune, not post-filter — assert the engine visited fewer nodes via `warren.last_stats()`).
- With a 10-hop variable segment on the root, `walk` finishes in < 1 s where the CTE exceeds 60 s on the same fixture — the seam-crossing demonstration from §9.1, recorded as a benchmark rather than asserted.

**Scale target.** 10M papers / 100M citations: the three-step walk < 2 s warm.

### A.5 Embedding propagation for retrieval (tier 1, matrices)

**Scenario.** Entities have pgvector embeddings; retrieval quality improves when each entity's embedding is smoothed with its typed neighbours' (a one-layer GNN without training). The smoothed matrix is written back to a pgvector column.

**Program.**

```sql
SELECT warren.vector_from_query('kg', 'emb', 'SELECT id, embedding::float4[] FROM entities', dtype := 'f32');
SELECT warren.aggregate_neighbours('kg', 'emb', 'emb_nbr', agg := 'mean', rels := ARRAY['related_to','part_of']);
SELECT warren.vector_map('kg', 'emb_smooth', '0.7 * emb + 0.3 * emb_nbr');   -- elementwise over width-d rows
UPDATE entities e SET embedding_smooth = r.row::vector
  FROM warren.matrix_rows('kg', 'emb_smooth') r WHERE r.node_id = e.id;
```

**Fixture.** 50,000 entities in 40 communities with 64-dim embeddings around community centroids plus noise; edges per the SBM in A.2.

**Expected outcome (assertions).**
- `aggregate_neighbours` equals a SQL `AVG` over neighbour rows grouped by node, elementwise within 1e-5 (f32).
- Mean within-community cosine similarity increases after smoothing and between-community similarity does not increase by more than 10% of that gain.
- `vector_map` on width-d matrices broadcasts scalars correctly and refuses mismatched widths with a clear error.

**Scale target.** 10M entities, d = 768, f32: `aggregate_neighbours` < 5 min; round trip to and from the pgvector column < 10 min.

### A.6 What the examples imply for the API

Writing these out surfaced several requirements that are now in the body of the note but are worth listing, since each is a test:

- `warren.id_map()` and `warren.vector_dense_id()` — seeding label propagation needs each node's own ID.
- `warren.explain_map(expr)` — tests assert that library expressions take the compiled path.
- `warren.last_stats()` — nodes visited, edges traversed, path taken, for the last call in the session; needed to assert pruning and to record benchmarks.
- `vector_map` must broadcast over width-d matrices, not just width-1 vectors.
- `gather` must accept a SQL string, a relation name and a vector name as its message source, and `collect` must preserve element types (`text[]` in, `text[][]`-equivalent out, exposed as `anyarray`). *M5:* `collect` follows `array_agg`, so `text[]` messages collect into a two-dimensional `text[]` of equal-length rows; ragged sets are sent one element per message (§6.4).
- Cross-graph `distance` aligned by external ID is required by the shadow-schema pattern and is not optional.
