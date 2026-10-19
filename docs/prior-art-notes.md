# Prior-art notes (DESIGN.MD §4A)

Mandatory pre-M1 reading, done October 2026. Source code was read only; none of it was built or run, and no code was copied into this repository.

| Project | Repository | Commit read |
|---|---|---|
| pgGraph (Evokoa, Apache-2.0, Rust/pgrx) | https://github.com/evokoa/pggraph | `b0f53df41cd9d4037df031a912e83ad948cf4d91` (`v1.2.1-1-gb0f53df`, 2026-09-23) |
| OneSparse (Michel Pelletier, Apache-2.0, C) | https://github.com/OneSparse/OneSparse | `79a57cd09930d6c7104c879c4859ba2221215703` (`v1.0.0-6-g79a57cd`, 2026-10-14; control file says 1.0.1) |

Paths below are relative to each repository root. pgGraph is about 260K lines. The reading covered the core (`graph/src/{edge_store,builder,bfs,path_finder,connected_components,resource,safety,build_snapshot,sync_capture,lib}.rs`, `sql_facade/{runtime,traversal}.rs`), the contributor docs (`docs/contributor_guide/*.mdx`), and the commit history (607 commits, read as a stand-in for the issue tracker). The GQL/Cypher frontend, RLS visibility machinery and sync/projection subsystems were skimmed, not read line by line. OneSparse (23K lines of C, mostly generated) was read in full for `src/onesparse.{c,h}`, `src/guc.c`, `src/matrix/`, and `src/graph/`. The generated operator tables were sampled.

---

## 1. pgGraph

### 1.1 What it is now (the §4A table is out of date)

pgGraph 1.2.1 is broader than the §4A row says. It has bounded GQL and openCypher read/write profiles (`graph.gql()`, `graph.cypher()`), trigger-based sync into backend-local overlays plus durable "projection segments", RLS/ACL/tenant-scoped visibility of results, union-find connected components, BFS/DFS, bidirectional BFS shortest path, Dijkstra, and dynamic background workers for builds. All of this sits in one crate, `graph/`. There is no engine/binding split: the engine calls `pgrx::check_for_interrupts!` directly (`graph/src/resource.rs:14-17`).

### 1.2 Structure and loading (§4A points 1 and 3)

**CSR layout.** One forward and one inbound CSR (`graph/src/edge_store.rs`). Each is a struct-of-arrays: `edge_offsets: u32[N+1]`, `targets: u32[]`, a parallel `type_ids` array whose width is 1, 2 or 4 bytes (chosen from the largest label ID, format v7), a `schema_reversed: u8[]`, optional `weights: u32[]`, and `relationship_ids` (one source-row identity per edge). All relationship types share one CSR. Traversal filters by type per edge inside the hot loop (`docs/contributor_guide/traversal-search-paths.mdx`, "Candidate Filtering Order"). Every file section starts on a 64-byte boundary, and the loader checks typed alignment before forming slices (`edge_store.rs:408-428`).

- *Right:* adaptive edge-type width, struct-of-arrays layout, alignment checks before `from_raw_parts`, and full CSR invariant validation on load (monotonic offsets, final offset = edge count, targets < N; `docs/contributor_guide/memory-model.mdx` "CSR invariants"). A fuzz target exists for the loader.
- *Wrong:* `u32` offsets and node indices cap a graph at 2³²−1 edges and nodes (`edge_store.rs:635`). Weights are `u32` integers: a `bigint` weight column is clamped to `[1, u32::MAX]` (`builder.rs:619`), so fractional weights cannot be represented. That is fine for hop costs and useless for linear algebra.

**Build: streaming, sorting, dangling edges, duplicates** (`graph/src/builder.rs`, `docs/contributor_guide/build-pipeline.mdx`).

- Nodes are read with SPI cursors in batches. Each `(table_oid, primary_key::text, node_idx)` row is spooled into `pg_temp.graph_build_nodes`.
- Edge endpoints are resolved in batches by an SPI join of `unnest($1::text[])` against that temp table (`builder.rs:958-990`). If an endpoint key matches more than one registered table, the lateral `HAVING count(*) = 1` (`builder.rs:978`) leaves it unresolved.
- Resolved edges go to a second temp table (`create_edge_spool`, `builder.rs:1204`). They are read back with `ORDER BY source, target, type_id, …` (`builder.rs:1232`), so Postgres's own sort (work_mem plus spill) does the sorting and a streaming `SortedEdgeStoreBuilder` builds the CSR. The persisted-build path later added its own checksummed external-sort run format (`build_runs.rs`).
- **Dangling edges** are silently skipped with no counter (`builder.rs:1035`, and the "Edge endpoint cannot be resolved → Edge row is skipped" row in build-pipeline.mdx).
- **Duplicates** are kept on purpose ("bag semantics"; `edge_store.rs:801-808`) because pgGraph returns edge identities and hydrates source rows.
- **Reverse CSR** is built from the forward CSR by a counting-sort transpose (`try_reversed`, `edge_store.rs:1058-1140`): count in-degrees, prefix-sum, scatter. No second sort is needed, and because sources are scanned in order, each reverse row comes out sorted.
- Throughput: the published Panama evidence is 5.8M edges built in 60 s, about 100K edges/s (`docs/release-notes.mdx:1002-1004`). Text-keyed SQL-join resolution is the likely bottleneck.

**Source consistency during a build** (`graph/src/build_snapshot.rs`). Before a build, pgGraph:
- takes `SHARE … NOWAIT` locks on every registered source table and partition (`:105`, `:199`), which blocks writers for the whole build;
- **rejects builds in a transaction that already holds write locks on a source** (`:119`), because "an uncommitted source change cannot escape through a filesystem artifact" (build-pipeline.mdx);
- for sync capture, pins one snapshot: `CommandCounterIncrement(); PushActiveSnapshot(GetTransactionSnapshot())`, then opens cursors with `SPI_cursor_open_with_args(…, read_only = true, …)` so every statement shares that snapshot, and pops it in a `PgTryBuilder::finally` (`graph/src/sync_capture.rs:45-60, 95-108`). The comment notes that pgrx's own SPI read-only mode depends on whether the transaction has an XID.

This is the clearest contrast with pg_warren. A file artifact cannot be rolled back, so pgGraph must forbid exactly the operation §8 depends on: building from the caller's uncommitted writes.

**Per-backend snapshot and invalidation.** A single `thread_local! static ENGINE: RefCell<Engine>` (`graph/src/lib.rs:919-920`) means one loaded graph per backend; selecting another graph drops it (`sql_facade/runtime.rs:605-623`). At each query start, pgGraph re-reads the catalog and compares the resident generation with the published one (`runtime.rs:641-650`); on a mismatch it reloads.
- Loading copies the file into an anonymous mapping, or into a sealed Linux `memfd` that other backends can adopt (`persistence/snapshot_cache.rs:152, 660`), and validates it fully.
- So it is **not** zero-copy mmap of the file. Commit `33687d6` moved to copying because "same-inode mutation or truncation" and drop-order bugs could invalidate typed views.
- No Postgres memory contexts are used. Everything is Rust heap or mmap, with a byte-accounting "governor" (`resource.rs`) checked against `graph.memory_limit_mb`.

**Frontier, depth and pagination** (`graph/src/bfs.rs`, `sql_facade/traversal.rs:29-60`).
- BFS uses a `VecDeque` frontier and a `RoaringBitmap` visited set.
- Parent, depth and edge-type metadata switch between dense and sparse storage: sparse when the visit budget is small relative to N (`SPARSE_METADATA_MIN_NODES = 4096`, ratio 16; `bfs.rs:32-33`). This avoids O(N) scratch memory for small traversals on big graphs.
- Circuit breakers are `max_depth` (default 5), `max_nodes` (100,000) and `max_frontier` (100,000). Defaults come from GUCs through SQL `DEFAULT` expressions such as `COALESCE(NULLIF(current_setting('graph.default_max_depth', true), '')::int, 5)` (`traversal.rs:40`).
- When a breaker trips, the call **returns partial rows, not an error**, flagged only by a `capped` column (`bfs.rs:1123, 1136`).
- Pagination is `max_rows` (default 1000) plus `row_offset`, so each page re-runs the traversal. SRFs build a full `Vec` and return `TableIterator::new(rows)`.
- Interrupts are checked every 1,024 expansions (`bfs.rs:1707`) through `postgres_error_as_rust_unwind` (`sql_visibility.rs:168-180`). That wrapper runs `CHECK_FOR_INTERRUPTS` inside `PgTryBuilder` and calls `resume_unwind` on error, so Rust destructors (including the `RefCell` borrow on `ENGINE`) run before Postgres takes over.

### 1.3 Bugs visible in the history (mistakes already made)

| Commit | Lesson for pg_warren |
|---|---|
| `fe546b4` path-finder: bidirectional BFS took the *first* meeting node in a level, not the one with the minimum combined distance, and so returned non-shortest paths | §9.3 `shortest_path` is bidirectional BFS. Finish scanning the level before choosing the meeting node, and add a differential proptest against single-direction BFS (they ran 20,000 cases). |
| `a648aa5` traversal: global uniqueness was applied after pagination | Deduplicate before any truncation or limit. |
| `39526c5` edge-store: out-of-range endpoints were silently dropped during CSR construction | Use fallible construction; a corrupt internal ID is an error, not a skip. |
| `63d8f4a` safety: errors were raised via direct `errfinish`, so Rust frames were not dropped | Raise errors only through pgrx (`ereport!`/`ErrorReport`); never call `pg_sys` error functions directly. |
| `with_panic_boundary` note (`sql_facade/admin.rs:1169-1173`) | Don't `catch_unwind` inside SPI or user-code paths; it can swallow pgrx's ERROR panics or erase the SQLSTATE. pgrx already installs the boundary around each `#[pg_extern]`. |
| `a6efee2` sql: INSERT inside a non-volatile SPI context failed | In pgrx 0.19, writes need `Spi::connect_mut` + `update()`. `Spi::connect` is read-only. |
| `33687d6` persistence: borrowed raw pointers into the mmap, drop-order bugs | Cached segments must own their memory (or hold an `Arc` to it), and views must never outlive it. |
| `ec98f4c` persistence: artifacts were not namespaced by database OID | A file-system store has to reimplement Postgres scoping. pg_warren's tables get this for free. |
| `7f0afd5` persistence: "publish graph generations transactionally — select committed generations through PostgreSQL MVCC, retain history for old snapshots"; reset/drop must keep files that older snapshots still need | After a year on files, they rebuilt transactional publication with catalog rows pointing at files. This strongly supports §5.1. |
| `2448293` persistence: recover interrupted graph replacements (statement_timeout, `pg_cancel_backend`) | Crash and cancel recovery is a large subsystem on files; with table storage it is a rollback. |
| `a3b31f8` PGDATA was not visible to backends; had to fall back to `DataDir` | Another file-only hazard. |
| `docs/user_guide/build-and-persistence.mdx:369` "does not provide standby support"; logical restore requires rebuild | Files are not replicated by WAL. §5.1's table store replicates to standbys. |

### 1.4 Why files over tables (§4A point 2)

The repository gives no quantitative rationale and never discusses a `bytea`/TOAST alternative. The stated reasons are "fast backend startup" (`persistence-format.mdx:9`) and, later, cross-backend sharing of one sealed copy on Linux. The measured costs are the bug list above, plus no standby support, plus forbidding builds from uncommitted state.

Since `33687d6` the loader **copies and fully validates** the file on every cold load anyway. Its remaining speed advantage over detoasting `STORAGE EXTERNAL` chunks is therefore sequential `read()` versus heap-plus-TOAST fetch. Both are O(bytes), and the gap is a small constant factor rather than an order of magnitude. Nothing here threatens §5.1. The one real advantage is sharing (sealed memfd), which corresponds to pg_warren's v1.1 DSM item, not to the persistence format. **Decision: keep the transactional table store.** M2 should measure cold-load MB/s from `warren.segments` and only bring §14 item 2 forward if the measurement is below about 300 MB/s.

### 1.5 pgrx usage (§4A point 3)

- **Version:** `pgrx = "=0.19.1"` and `pgrx-tests = "=0.19.1"`, exact pins (`graph/Cargo.toml:32`). Rust is pinned to `1.96.0` (`graph/rust-toolchain.toml`) with `rust-version = "1.96"`. Features run `pg13`…`pg18`, default `pg17`; PG13 is "legacy best-effort" (`Cargo.toml:14-25`). `panic = "unwind"` is set in both profiles (required). The release profile uses `lto = "fat"` and `codegen-units = 1`.
- **Lints:** `unsafe_op_in_unsafe_fn = deny`, `clippy::undocumented_unsafe_blocks = deny`, and `#![cfg_attr(not(any(test, feature = "pg_test")), deny(clippy::expect_used, clippy::panic, clippy::unwrap_used))]` (`lib.rs:11-14`). Unsafe slice code has Miri gates (commit `33687d6`), plus `release/unsafe-allowlist.json`.
- **Allocation:** every large `Vec` uses `try_reserve` and maps failure to a typed OOM error instead of aborting the backend (`edge_store.rs:690-700`).
- **Errors:** a `GraphError` enum with `report() -> !` that builds `ErrorReport::new(sqlstate, msg, …).set_detail(...).set_hint(...)` (`safety.rs:493-504`). Errors use standard SQLSTATEs; a stable project code goes in DETAIL.
- **SRFs:** `TableIterator<'static, (name!(…), …)>` over a fully built `Vec`. There are no lazy SRFs, which is safe but uses O(rows) memory.
- **GUCs:** `GucRegistry::define_*_guc` with `c"…"` literals (`config.rs:524+`). The bounded-integer GUCs are a good pattern: `memory_limit_mb` (Suset), `query_memory_mb` (Userset), `maintenance_memory_mb`.
- **Init:** `_PG_init` registers GUCs and `RegisterXactCallback`/`RegisterSubXactCallback` (`lib.rs:931-936`, `projection/tx_delta.rs:1201-1202`). Most features work without `shared_preload_libraries`.
- **Schema and upgrades:** `extension_sql_file!("../sql/bootstrap.sql", requires = [auto_discover])` (`lib.rs:908`) carries hand-written catalog DDL. Upgrade scripts are hand-written and additive (`graph/sql/graph--1.1.0--1.2.0.sql`). The release gate diffs `cargo pgrx schema` against a frozen `release/v1-schema.sql` (`tests/heavy/run_release_gate.sh:83`) and runs `ALTER EXTENSION graph UPDATE` from the previous release (`tests/heavy/publication_upgrade.sh`). Catalog tables are registered with `pg_extension_config_dump` (`graph/sql/bootstrap.sql:934-936`) so `pg_dump` includes them.
- **Multi-version CI:** one Docker image installs `postgresql-NN` and `postgresql-server-dev-NN` for 14–18 from apt.postgresql.org. It runs `cargo pgrx init --pgNN=/usr/lib/postgresql/NN/bin/pg_config` against the distro builds, so pgrx never downloads or compiles Postgres. It installs `cargo-pgrx --version "$PGRX_VERSION" --locked` and asserts the version. It then loops `cargo test --release --no-default-features --features "pgNN development"` and `RUST_TEST_THREADS=1 cargo pgrx test pgNN` (`graph/tests/heavy/Dockerfile.pg-matrix:4-11, 37-45, 90-167`; `run_pg_matrix.sh`). The base image is pinned by digest. Serial pgrx tests avoid flakiness on the shared test cluster.

### 1.6 Summary for pgGraph

**Got right:** derived CSR with forward plus transposed reverse; struct-of-arrays with adaptive type width; validate everything before forming unsafe slices; resource accounting with `try_reserve`; breakers on depth/nodes/frontier; sparse per-call metadata; error discipline through pgrx; strong multi-version and upgrade testing.

**Got wrong (for our purposes):**
- file persistence and the long tail of bugs it caused;
- `u32` caps on nodes and edges, and integer-only weights;
- text-keyed endpoint resolution through SQL joins (about 100K edges/s);
- silently dropped dangling edges with no count;
- source-table `SHARE` locks for the whole build;
- partial results on breaker trips signalled only by a column;
- OFFSET pagination that re-runs the traversal;
- one graph per backend;
- no engine/binding split.

**pg_warren differs:** transactional `bytea` segments (§5.1); `u64` offsets and `u32`/`u64` targets; `f64` weights; typed `id_map` resolved in Rust; `dropped_edges` counted; MVCC snapshot instead of locks; staged builds that *see* uncommitted writes; many cached versions per backend; `warren-core` without Postgres.

---

## 2. OneSparse

### 2.1 Kernels and vocabulary (§4A point 4)

OneSparse is a thin C binding over SuiteSparse:GraphBLAS 10.5.1 and LAGraph 1.2.2 (`versions.env`). Every operation is a GraphBLAS call. `mxv` is typical (`src/matrix/matrix_mxv.c:27-60`): `mxv(a matrix, b vector, op semiring = NULL, c vector = NULL, mask vector = NULL, accum binaryop = NULL, descr descriptor = NULL)` maps to `GrB_mxv(c, mask, accum, semiring, A, b, descr)`. The defaults are `plus_times` for numeric types and `any_pair` for bool (`src/onesparse.c`, `default_semiring`).

- **Operation names:** `mxm`, `mxv`, `vxm`, `eadd`, `emult`, `eunion`, `apply`, `choose` (GraphBLAS `select`), `reduce_scalar`, `reduce_rows`/`reduce_cols`, `assign`, `extract`, `kron`, `transpose`, `nvals`, `elements`. There are operators too: `@` for mxm/mxv/vxm, `&+ &* &- &/` for emult, and scalar `+ - * /`.
- **Semiring names** are `<add>_<mult>_<type>`: `plus_times_fp64`, `min_plus_int64`, `max_times_fp64`, `any_pair_bool`, `plus_pair_int64`, `min_first`, `min_second`, `lor_land`, and so on (`src/semiring/semiring_header.h`, generated). The multiply operator takes `(a_ij, x_j)`: `first` = matrix entry (edge weight) and `second` = vector entry.
- **Descriptors** are flag strings: `r` (replace output outside the mask), `c` (complement mask), `s` (structural mask), `t0`/`t1` (transpose input 0/1), and combinations such as `rsct0` (`src/descriptor/descriptor_header.h`).
- **Accumulators** are binary operators (`plus`, `min`, `second`, …) applied as `c = c ⊙ result`.
- **Performance envelope:** the project blog reports BFS over 7 billion edges/s on GAP graphs up to 4.3B edges (`docs/blog/posts/billions-eps-onesparse-postgres.md:1566-1572`). That comes from SuiteSparse's direction-optimising BFS and JIT kernels on a large multicore host whose hardware is not stated. It is the ceiling for tier-1 SpMV/BFS, not a v1 target.

**Interrupts and threads.** There are **no** `CHECK_FOR_INTERRUPTS` calls anywhere in `src/` (grep count 0). A long `mxm` or PageRank therefore cannot be cancelled. GraphBLAS runs OpenMP threads inside the backend with no thread-count GUC; the only GUCs are `onesparse.burble` and `onesparse.jit_control` (`src/guc.c:117-140`). The burble hook calls `ereport(NOTICE)` from inside GraphBLAS (`guc.c:16-37`).

### 2.2 Matrices as datatypes (§4A point 5)

- `matrix`, `vector`, `scalar` and `graph` are expanded objects (`ExpandedObjectHeader` + `GrB_Matrix`; `src/matrix/matrix.h:11-20`). Each lives in its own `AllocSetContext`, and a `MemoryContextRegisterResetCallback` frees the GraphBLAS object when the context resets (`matrix.c:89-140`). That is a correct way to tie a foreign allocation to Postgres lifetimes, error paths included.
- Flattening serialises with `GxB_Matrix_serialize` into one varlena (`matrix.c:16-44`). Every SQL call that receives a stored (flat) value deserialises it again. When the same datum is passed as several arguments, expansion is shared through a `flat_datum_pointer` comparison (`DatumGetMatrixMaybeA/AB/ABC`, `matrix.c:204-296`).
- **PG18-only, and why:** each mutating function has a planner support function for `SupportRequestModifyInPlace` (`src/onesparse.h:146-163`, macro `SUPPORT_FN`), which is new in PG18. It lets PL/pgSQL `x := mxv(…, x)` update an expanded object in place instead of copying it. The blog says "support for Postgres less than 18 is not practical" (`billions-eps…md:4624-4628`).
- **Size ceiling:** one varlena is at most 1 GB, which the author puts at "a few hundred million edges at most" (`billions-eps…md:265-279`). Larger graphs go through large objects (`matrix_save`/`matrix_load`) or server-side files (`serialize_file`, `mmread`).
- **Loading from SQL exists.** The §4A claim of "Matrix Market only" is wrong: `matrix_query(query text, nrows, ncols, batch_size)` (`onesparse--1.0.1.sql:1775`, `src/matrix/matrix_query.c`) reads `(i bigint, j bigint, v)` through an SPI cursor. Duplicates resolve last-wins via a `SECOND` accumulator (`:96, :163`), and when `nrows = -1` the dimension is `GrB_INDEX_MAX + 1` (hypersparse, `:35`), so raw `bigint` keys can be used as indices with no ID map. There are also `matrix_agg`/`vector_agg` aggregates.
- **The `graph` type drops its caches on flatten.** `graph_pagerank` calls `LAGraph_Cached_AT` and `LAGraph_Cached_OutDegree` on each invocation (`src/graph/graph_pagerank.c:25-26`), but flattening serialises only `A` (`src/graph/graph.c:33`). Any stored graph therefore recomputes its transpose on every call.

### 2.3 Bugs and hazards seen

These were found by reading only, not confirmed by running.

- `matrix_load.c:26`: the size comes from `lo_lseek` via `DatumGetInt32`, so objects over 2 GB are mis-sized. `lo_lseek64` is the correct call.
- `matrix_save.c:37`: `lo_write(fd, data, size)`. `lo_write` takes an `int` length and the return value is unchecked, so writes over 2 GB truncate silently. The buffer from `GxB_Matrix_serialize` is also never freed (compare the commented-out free in `matrix.c:76-84`).
- Commit `56ed1f1`, "fix aggregate memory context bug": aggregate state was allocated in `CurrentMemoryContext` rather than the context from `AggCheckCallContext`. This is a classic mistake, now fixed (`matrix_agg_matrix.c:26`).
- There are no upgrade scripts (only `onesparse--1.0.1.sql`), and the control-file comment is still template text.

### 2.4 Summary for OneSparse

**Got right:** GraphBLAS vocabulary (semiring, mask, accum, descriptor) exposed almost unchanged, so the names are standard; expanded objects with reset-callback cleanup; argument de-duplication for expansions; the PG18 in-place support function; documentation and regression tests generated from the same SQL files (`doctestify.py`).

**Got wrong (for our purposes):**
- values-as-datatypes cap stored graphs at 1 GB and force a deserialise on every call;
- cached transposes and degrees are lost whenever a graph is stored;
- operations cannot be cancelled and run with uncapped OpenMP threads and unaccounted `malloc` inside a backend;
- PG18-only;
- no ID mapping (users supply dense or hypersparse integer indices), no versions, no catalog.

**pg_warren differs:** vectors are named and persisted in chunks rather than passed as values, which removes the 1 GB limit and keeps the reverse CSR; kernels check a cancel flag and run under `warren.threads` and `warren.max_compute_memory`; PG15–18. pg_warren adopts GraphBLAS names so users can move between the two.

---

## 3. Concrete recommendations for pg_warren

### 3.1 pgrx patterns to copy (paraphrased, not copied)

1. **Pin the toolchain exactly.** `pgrx = "=0.19.1"`, `pgrx-tests = "=0.19.1"` (re-check the latest 0.19.x at M0; this is what pgGraph ships against PG14–18), plus `rust-toolchain.toml`. Use features `pg15`…`pg18` with `default = ["pg17"]`, and build per major with `--no-default-features --features pgNN`. Set `panic = "unwind"` in all profiles.
2. **Run CI against distro Postgres.** Install `postgresql-NN` and `postgresql-server-dev-NN` from apt.postgresql.org, run `cargo pgrx init --pgNN=/usr/lib/postgresql/NN/bin/pg_config` for each major, and `cargo install cargo-pgrx --version <pin> --locked`. Run `cargo pgrx test pgNN` with `RUST_TEST_THREADS=1`. This is faster than letting pgrx compile Postgres.
3. **Lints.** `unsafe_op_in_unsafe_fn = "deny"`, `clippy::undocumented_unsafe_blocks = "deny"`, and deny `unwrap_used`/`expect_used`/`panic` outside `cfg(test)`. Run Miri on `warren-core`'s unsafe slice code.
4. **One error path.** Use a `WarrenError` enum with `fn report(self) -> !` that builds a pgrx `ErrorReport` with a standard SQLSTATE plus DETAIL/HINT. Never call `pg_sys::errfinish`/`elog` directly, and never `catch_unwind` around SPI.
5. **Interrupts.** On the main thread, between kernel chunks, call `pgrx::check_for_interrupts!()` inside a `PgTryBuilder` wrapper that turns the longjmp into `resume_unwind`, so destructors and `RefCell` borrows release before Postgres handles the error. Worker threads check only the atomic cancel flag (as §6.9 already says).
6. **Fallible allocation.** Call `try_reserve`/`try_reserve_exact` on every graph-sized buffer and map failures to a `53200 out_of_memory` error accounted against `warren.max_compute_memory`.
7. **Pin one snapshot for a refresh.**
   ```text
   PgTryBuilder::new(|| {
       CommandCounterIncrement();                              // see own earlier writes
       PushActiveSnapshot(GetTransactionSnapshot());
       let nodes = SPI_cursor_open_with_args(.., read_only = true, ..);
       let edges = SPI_cursor_open_with_args(.., read_only = true, ..);
       ... stream both ...
   }).finally(|| PopActiveSnapshot()).execute()
   ```
   Under READ COMMITTED, separate non-read-only SPI statements each take a fresh snapshot, which would make `node_query` and `edge_query` inconsistent. Writes to `warren.segments` then go through `Spi::connect_mut` + `update()`.
8. **GUC-backed defaults in the SQL signature.** For example, `max_results default!(Option<i64>, "NULLIF(current_setting('warren.max_results', true), '')::bigint")` lets an operator change defaults without a new release.
9. **Schema discipline.** Put catalog DDL in `extension_sql_file!` (bootstrap, `requires = [...]`). Call `pg_extension_config_dump('warren.graphs', '')` and the equivalent for every catalog table and its sequences, or `pg_dump` will silently omit graph definitions, versions and vectors. Upgrade scripts are hand-written. In CI: (a) diff `cargo pgrx schema` against the committed `sql/pg_warren--X.Y.Z.sql`; (b) install the previous release, run `ALTER EXTENSION pg_warren UPDATE`, and compare `pg_proc`/`pg_class` signatures with a fresh install.
10. **Expanded-object hygiene, if we ever add a type.** Tie foreign allocations to a child `MemoryContext` with a reset callback (OneSparse `matrix.c:131-136`), and allocate aggregate state in the `AggCheckCallContext` context.

### 3.2 Engine choices informed by the reading

- **Reverse CSR by transpose, not by a second sort.** Build the reverse CSR from the finished forward CSR with a counting-sort transpose: count in-degrees, prefix-sum, scatter while scanning sources in order. Rows come out sorted by source, and the extra pass is O(N+M).
- **Forward CSR without a comparison sort.** Dense IDs are known before edges are read, so `warren-core` can do: remap → counting sort by `src` → per-row sort of `(rel, dst)` → deduplicate within the row. The tuplesort FFI (not wrapped by pgrx) is then not needed for v1; add an external-run spill only if the M2 memory measurements require it. Peak memory at 100M weighted edges is about 1.8 GB for the edge list plus 1.2 GB forward; the edge list is freed before the transpose.
- **ID remapping in Rust.** Resolve IDs by binary search or a hash over the typed `id_map`. Avoid text keys and SQL joins: pgGraph's text-keyed join resolution runs at about 100K edges/s, and §12's target (100M edges in under 8 min) needs at least 2× that.
- **Count everything dropped.** `dropped_edges` covers unknown endpoints and NULL endpoints; add `merged_edges` for parallel edges folded by dedup.
- **Bidirectional BFS:** finish the level, then pick the meeting node with the minimum combined distance. Add a differential proptest against single-direction BFS.
- **Adaptive per-call scratch:** dense visited/parent/depth arrays when the expected visit budget exceeds about `id_space/16`, otherwise sparse (hash or roaring).
- **Breakers must be visible.** When `max_results`/`max_depth` truncates, set `truncated = true` in `warren.last_stats()` and raise a `NOTICE`. Never truncate silently.
- **No OFFSET pagination.** Stream from the SRF and let SQL `LIMIT` stop it. The SRF iterator holds an `Arc` to the cached version, so a concurrent `warren.evict` cannot invalidate it.
- **Canonical component IDs.** After union-find, relabel each component to its minimum dense ID, so results are deterministic and comparable across versions (pgGraph's union-find roots are not; `connected_components.rs:3-14`).
- **Validate segments on load.** Check monotonic offsets, final offset = M, targets < `id_space`, and alignment before forming slices (O(N+M), cheap next to a detoast). Add a `cargo fuzz` target for the segment decoder.
- **Cache memory.** Use the Rust allocator (or an anonymous mapping for page alignment), **not** `TopMemoryContext`. `palloc` caps a single chunk at 1 GB (`MaxAllocSize`), the forward `targets` of a 100M-edge graph is 0.4–0.8 GB and grows past 1 GB, and memory reached from worker threads should not be palloc-owned.
- **Direction-optimising BFS** (push/pull switch when the frontier exceeds about N/20 and reverse CSR exists) for `reachable_set` and `components`. This is how GraphBLAS reaches multi-GTEPS; schedule it in M7 rather than M1.
- **Security:** follow REFRESH MATERIALIZED VIEW semantics. `refresh` runs the stored `node_query`/`edge_query` as the **graph owner** (`SetUserIdAndSecContext`), never as a SECURITY DEFINER extension owner. Traversal functions require a graph-level privilege, because topology can leak past source-table RLS. pgGraph spent many commits (`a9d18b5`, `e186df9`, `63f768d`, `f69771c`) on definer and ACL holes.

### 3.3 DESIGN.MD sections that should change (proposed wording)

1. **§4A table, pgGraph row, "What it is" and "Divergence"**. Append to "What it is":
   > "1.2.1 also ships bounded GQL/openCypher profiles (`graph.gql`, `graph.cypher`), trigger-based sync into backend-local overlays, RLS/tenant-scoped results, union-find components and integer-weighted Dijkstra."

   Replace "No versioning … documented" with:
   > "Generations are published through MVCC catalog rows that point at files (since 1.2.1), but there are no user-visible versions, no stable dense IDs across rebuilds, no standby support, and builds are refused inside transactions holding source write locks. Weights are `u32` integers; node and edge counts are capped at 2³²."

2. **§4A table, OneSparse row.** Replace "(loading documented only from Matrix Market files)" with:
   > "(loads from SQL via `matrix_query`/`matrix_agg`, from large objects, or from server files; a stored matrix is one varlena, ≤1 GB)"

   Replace "PG18-only is a hard constraint" with:
   > "PG18-only because it relies on PG18's `SupportRequestModifyInPlace` for in-place PL/pgSQL updates of expanded objects; a hard constraint against the PG 15–18 target."

   Version: 1.0.1.

3. **§5.1, after the table DDL**, add:
   > "The bootstrap SQL registers every `warren.*` catalog table and sequence with `pg_extension_config_dump`, so logical dumps include graph definitions, versions, segments and vectors. `warren.graphs` gains `owner regrole NOT NULL DEFAULT current_user`."

4. **§5.3, last paragraph**, add:
   > "On load, every segment is validated (alignment, monotonic offsets, final offset = M, all targets < id_space) before any unchecked slice is formed; the decoder is fuzzed. Whether per-relation CSRs or a single merged CSR with a parallel adaptive-width (`u8`/`u16`/`u32`) relation-ID array is the primary structure is decided by an M1 benchmark: the merged form halves storage when `has_relations`, at the cost of a per-edge relation test."

5. **§5.4, steps 1–4.** Replace step 1 with:
   > "Push one snapshot for the whole build (`CommandCounterIncrement` then `PushActiveSnapshot(GetTransactionSnapshot())`) and run `node_query` and `edge_query` as read-only SPI cursors under it, executing them with the graph owner's privileges. No locks are taken on source tables."

   In steps 2–4 replace "stream … into a `tuplesort`" / "Tuplesort on `(rel, src, dst)`" with:
   > "counting-sort by dense `src` in `warren-core`, then sort and deduplicate each row by `(rel, dst)`; NULL or unknown endpoints are counted in `dropped_edges`, merged parallel edges in a new `merged_edges` column."

   And replace "Repeat sorted on `(rel, dst, src)` for reverse CSR" with:
   > "Build the reverse CSR by transposing the forward CSR (counting sort over targets, O(N+M), rows emerge sorted)."

   Keep the 4 GB memory target, and note that an external-sort spill is added only if M2 measurements require it.

6. **§5.5.** Replace "allocate contiguous segment buffers in `TopMemoryContext`" with:
   > "allocate contiguous, 64-byte-aligned segment buffers with the Rust allocator (palloc caps single chunks at 1 GB), owned by an `Arc<VersionData>` in a backend-local map keyed by `version_id`; SRF iterators hold an `Arc` clone so eviction mid-scan is safe."

   Add:
   > "The `current_version` lookup uses a saved SPI plan."

7. **§6.3 semiring list.** Replace the comment with:
   > "`plus_times | min_plus | max_plus | max_times | min_times | lor_land | any_pair | plus_pair | min_first | min_second | max_second | plus_first | plus_second` (GraphBLAS names: `<add>_<mult>`; `⊗(a_ij, x_j)`, so `first` = the edge weight and `second` = the input vector value; `or_and` accepted as an alias of `lor_land`)."

   Add a parameter `replace bool DEFAULT false`, defined as:
   > "with a mask, entries of y outside the mask are kept unless replace (GraphBLAS `r` descriptor)."

   Add:
   > "`accumulate` takes a binary-op name (`plus`, `min`, `max`, `times`, `first`, `second`)."

   Check the §6.7 library uses `min_second` for min-label propagation.

8. **§6.9.** Add:
   > "The main thread's interrupt check is wrapped so that a Postgres ERROR becomes a Rust unwind (destructors run) before control returns to Postgres; errors are raised only through pgrx `ErrorReport`. Graph-sized buffers use `try_reserve`."

9. **§7.** Add:
   > "Access: traversal and compute functions require `USAGE` on the graph (recorded in `warren.graphs`); a graph exposes topology computed with the owner's visibility, exactly like a materialised view."

10. **§9.3, implementation notes.** Add:
    > "`shortest_path` (bidirectional BFS) completes the current level before choosing the meeting node with minimum combined distance, with a differential proptest against single-direction BFS. Per-call scratch is dense or sparse depending on the visit budget. Hitting `max_depth`/`max_results` sets `truncated` in `warren.last_stats()` and raises a NOTICE. `components` labels each component by its minimum dense ID. There is no OFFSET pagination; SRFs stream."

11. **§11.** Replace "Rust via pgrx" details with:
    > "pgrx pinned exactly (`=0.19.x`, verified at M0) with `rust-toolchain.toml`; features `pg15`–`pg18`; `panic = "unwind"`; lints `unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks` and no `unwrap`/`expect`/`panic` outside tests; Miri on `warren-core` unsafe code."

12. **§12 Correctness.** Add:
    > "Bidirectional-vs-unidirectional shortest-path differential proptest; `cargo fuzz` target for the segment decoder; upgrade-path test (`ALTER EXTENSION UPDATE` from the previous release equals a fresh install's catalog) and `cargo pgrx schema` drift check."

13. **§13 CI line.** Replace with:
    > "fmt, clippy, unit, Miri (warren-core), pgrx tests PG 15–18 against PGDG packages (`cargo pgrx init --pgNN=<pg_config>`, `RUST_TEST_THREADS=1`), schema-drift and upgrade-path checks, bench smoke test."

14. **§14 item 2.** Append:
    > "pgGraph evidence (§4A notes): its file artifacts are now copied and validated on every load anyway, needed MVCC catalog publication bolted on later, and lack standby support. Keep `bytea`. Measure cold load in M2 and revisit only if below ~300 MB/s. Linux sealed-memfd sharing is an alternative to DSM for v1.1."

15. **§14 item 4.** Append:
    > "Record merged duplicates in `versions.merged_edges`. pgGraph keeps duplicates only because it returns edge identities, which pg_warren does not."

16. **§14 item 16.** Mark done:
    > "Done 2026-10: see `docs/prior-art-notes.md` (pgGraph `b0f53df`, OneSparse `79a57cd`). Sections changed: §4A, §5.1, §5.3, §5.4, §5.5, §6.3, §6.9, §7, §9.3, §11, §12, §13, §14 items 2 and 4."

17. **§10.** Add a bullet:
    > "**Direction-optimising BFS** (push/pull) for `reachable_set` and `components`."

---

## 4. Positioning (README paragraph)

> pg_warren is a graph index and compute engine for PostgreSQL. It derives a compressed-sparse-row adjacency structure from your ordinary tables and runs deep traversals and whole-graph linear algebra over it from SQL. It is not the first extension to put a CSR beside Postgres tables: pgGraph pioneered derived, rebuildable CSR traversal behind SQL functions, and OneSparse brought the GraphBLAS semiring model into Postgres as datatypes. pg_warren's contribution is different. Every build is a versioned snapshot with a stable dense-ID space, stored transactionally in ordinary tables (so it is WAL-logged, replicated and rolled back with your transaction). A version can be built *inside* the transaction that changes the data and measured against the current one, structurally or spectrally, before you commit. One CSR serves both traversal and semiring compute, control flow stays in PL/pgSQL with per-node logic written as SQL expressions, and contraction gives community-level computation. pg_warren takes no code from either project; ideas adopted from them are credited in `NOTICE` and in comments where they are used.
