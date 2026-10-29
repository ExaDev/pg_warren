//! `warren.cache()` (DESIGN.md §5.7, §14 item 49): what this backend's
//! private cache holds, a row per cached version, persisted vector and
//! transient vector of the current role, then a `backend` row counting
//! every role's entries against `warren.max_cache_memory`.
//!
//! An entry's `state` is `in use` while a call holds it, `pinned` while the
//! current statement pins it and no call holds it (an idle pin, §5.5),
//! `idle` otherwise, and `held` for a version evicted that a call still
//! holds. Its `last_use` is microseconds since the backend started, so
//! the least recently used has the smallest; the `backend` row's is now.
//!
//! Privacy is `warren.shared_cache()`'s: rows only for graphs the caller
//! may read, as the statement's snapshot sees them, and never another
//! role's transient vectors. `PARALLEL RESTRICTED`: it is about the
//! leader's session, like `prewarm`.

use std::collections::HashMap;
use std::time::Instant;

use pgrx::pg_sys::{self, Oid};
use pgrx::prelude::*;

use super::bound::{cached_bytes, max_cache_memory};
use super::statement::{vector_pinned, version_pinned};
use super::versions::CACHE;
use super::CacheError;
use crate::access::{as_catalog_owner, entry, is_reader};
use crate::catalog::VersionId;
use crate::read_only::select;
use crate::vector::{cached_vector_listing, role_transients};

/// What an entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Kind {
    Version,
    Vector,
    Transient,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Version => "version",
            Self::Vector => "vector",
            Self::Transient => "transient",
        }
    }
}

/// An entry's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    InUse,
    Pinned,
    Idle,
    Held,
}

impl State {
    /// The state of an entry that is `cached` (not merely held by a call),
    /// `in_use` by a call and `pinned` by the statement.
    fn of(cached: bool, in_use: bool, pinned: bool) -> Self {
        match (cached, in_use, pinned) {
            (false, _, _) => Self::Held,
            (true, true, _) => Self::InUse,
            (true, false, true) => Self::Pinned,
            (true, false, false) => Self::Idle,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::InUse => "in use",
            Self::Pinned => "pinned",
            Self::Idle => "idle",
            Self::Held => "held",
        }
    }
}

/// An entry of the cache, before the catalog names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Entry {
    pub(crate) kind: Kind,
    pub(crate) graph_id: i32,
    /// A version's or transient vector's version; a persisted vector's
    /// comes from the catalog.
    pub(crate) version: Option<VersionId>,
    /// A transient vector's name; a persisted vector's comes from the
    /// catalog.
    pub(crate) vector: Option<String>,
    pub(crate) vector_id: Option<i64>,
    pub(crate) bytes: u64,
    pub(crate) state: State,
    pub(crate) last_use: Option<i64>,
}

/// What the catalog says of the entries' graphs and persisted vectors.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    /// Each graph's name and whether the caller may read it.
    pub(crate) graphs: HashMap<i32, (String, bool)>,
    /// Each persisted vector's version and name.
    pub(crate) vectors: HashMap<i64, (VersionId, String)>,
}

/// One row of `warren.cache()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub(crate) kind: &'static str,
    pub(crate) graph: Option<String>,
    pub(crate) version: Option<VersionId>,
    pub(crate) vector: Option<String>,
    pub(crate) vector_id: Option<i64>,
    pub(crate) bytes: u64,
    pub(crate) state: Option<&'static str>,
    pub(crate) last_use: Option<i64>,
    pub(crate) cap: Option<u64>,
}

/// The rows of `entries` that `catalog` names and the caller may read, by
/// graph, version, kind and vector, then the `backend` row: `total` bytes
/// held against `cap` at `now`.
pub(crate) fn rows(
    entries: Vec<Entry>,
    catalog: &Catalog,
    total: u64,
    cap: u64,
    now: i64,
) -> Vec<Row> {
    let mut rows: Vec<(Kind, Row)> = entries
        .into_iter()
        .filter_map(|entry| {
            let (graph, readable) = catalog.graphs.get(&entry.graph_id)?;
            if !readable {
                return None;
            }
            let (version, vector) = match (entry.kind, entry.vector_id) {
                (Kind::Vector, Some(id)) => {
                    let (version, name) = catalog.vectors.get(&id)?;
                    (Some(*version), Some(name.clone()))
                }
                _ => (entry.version, entry.vector),
            };
            let row = Row {
                kind: entry.kind.name(),
                graph: Some(graph.clone()),
                version,
                vector,
                vector_id: entry.vector_id,
                bytes: entry.bytes,
                state: Some(entry.state.name()),
                last_use: entry.last_use,
                cap: None,
            };
            Some((entry.kind, row))
        })
        .collect();
    rows.sort_by(|(a_kind, a), (b_kind, b)| {
        (&a.graph, a.version, a_kind, &a.vector).cmp(&(&b.graph, b.version, b_kind, &b.vector))
    });
    let mut rows: Vec<Row> = rows.into_iter().map(|(_, row)| row).collect();
    rows.push(Row {
        kind: "backend",
        graph: None,
        version: None,
        vector: None,
        vector_id: None,
        bytes: total,
        state: None,
        last_use: Some(now),
        cap: Some(cap),
    });
    rows
}

/// Microseconds since this backend started, now and at an earlier instant.
struct Clock {
    now: Instant,
    since_start: i64,
}

impl Clock {
    fn new() -> Self {
        // SAFETY: the current time and this backend's start, on the main
        // thread.
        let since_start = unsafe { pg_sys::GetCurrentTimestamp() - pg_sys::MyStartTimestamp };
        Self { now: Instant::now(), since_start }
    }

    fn at(&self, when: Instant) -> i64 {
        let ago = i64::try_from(self.now.duration_since(when).as_micros()).unwrap_or(i64::MAX);
        self.since_start.saturating_sub(ago)
    }
}

/// The entries the caches hold now, the current role's transient vectors
/// only.
fn entries(clock: &Clock) -> Vec<Entry> {
    let versions = CACHE.with_borrow(|cache| cache.listing()).into_iter().map(|listed| Entry {
        kind: Kind::Version,
        graph_id: listed.graph_id,
        version: Some(listed.version),
        vector: None,
        vector_id: None,
        bytes: listed.bytes,
        state: State::of(listed.last_used.is_some(), listed.in_use, version_pinned(listed.version)),
        last_use: listed.last_used.map(|when| clock.at(when)),
    });
    let vectors =
        cached_vector_listing().into_iter().map(|(id, graph_id, bytes, when, in_use)| Entry {
            kind: Kind::Vector,
            graph_id,
            version: None,
            vector: None,
            vector_id: Some(id),
            bytes,
            state: State::of(true, in_use, vector_pinned(id)),
            last_use: Some(clock.at(when)),
        });
    let transients =
        role_transients().into_iter().map(|(graph_id, version, name, bytes, in_use)| Entry {
            kind: Kind::Transient,
            graph_id,
            version: Some(version),
            vector: Some(name),
            vector_id: None,
            bytes,
            state: State::of(true, in_use, false),
            last_use: None,
        });
    versions.chain(vectors).chain(transients).collect()
}

/// What the catalog says of `entries`, under the statement's snapshot.
fn catalog(entries: &[Entry]) -> Result<Catalog, CacheError> {
    let mut graph_ids: Vec<i32> = entries.iter().map(|entry| entry.graph_id).collect();
    graph_ids.sort_unstable();
    graph_ids.dedup();
    let vector_ids: Vec<i64> = entries.iter().filter_map(|entry| entry.vector_id).collect();
    let mut catalog = Catalog::default();
    if graph_ids.is_empty() {
        return Ok(catalog);
    }
    let mut graphs = Vec::new();
    as_catalog_owner(|| {
        Spi::connect(|client| {
            let query = "SELECT graph_id, name, owner::oid, readers::oid[] FROM warren.graphs
                         WHERE graph_id = ANY ($1)";
            select(client, query, None, &[graph_ids.into()], |rows| {
                for row in rows {
                    let id: i32 = row.get(1)?.unwrap_or_default();
                    let name: String = row.get(2)?.unwrap_or_default();
                    let owner: Oid = row.get(3)?.unwrap_or(Oid::INVALID);
                    let readers: Vec<Oid> = row.get(4)?.unwrap_or_default();
                    graphs.push((id, name, owner, readers));
                }
                Ok::<_, CacheError>(())
            })?;
            if vector_ids.is_empty() {
                return Ok(());
            }
            let vectors = "SELECT vector_id, version_id, name FROM warren.vectors
                           WHERE vector_id = ANY ($1)";
            select(client, vectors, None, &[vector_ids.into()], |rows| {
                for row in rows {
                    let id: i64 = row.get(1)?.unwrap_or_default();
                    let version: VersionId = row.get(2)?.unwrap_or_default();
                    let name: String = row.get(3)?.unwrap_or_default();
                    catalog.vectors.insert(id, (version, name));
                }
                Ok::<_, CacheError>(())
            })
        })
    })?;
    // As the caller, not the catalog owner.
    for (id, name, owner, readers) in graphs {
        catalog.graphs.insert(id, (name, is_reader(owner, &readers)));
    }
    Ok(catalog)
}

/// A size as `bigint`.
fn count(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// `warren.cache()`: see the module's documentation.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
// pgrx reads the columns' names from the return type itself, so they
// cannot be factored into a type alias.
#[allow(clippy::type_complexity)]
fn cache() -> TableIterator<
    'static,
    (
        name!(kind, String),
        name!(graph, Option<String>),
        name!(version, Option<i64>),
        name!(vector, Option<String>),
        name!(vector_id, Option<i64>),
        name!(bytes, i64),
        name!(state, Option<String>),
        name!(last_use, Option<i64>),
        name!(cap, Option<i64>),
    ),
> {
    entry(|_| {
        let clock = Clock::new();
        let entries = entries(&clock);
        let catalog = catalog(&entries)?;
        let rows =
            rows(entries, &catalog, cached_bytes(), max_cache_memory().value, clock.since_start);
        let rows = rows.into_iter().map(|row| {
            (
                row.kind.to_owned(),
                row.graph,
                row.version,
                row.vector,
                row.vector_id,
                count(row.bytes),
                row.state.map(str::to_owned),
                row.last_use,
                row.cap.map(count),
            )
        });
        Ok::<_, CacheError>(TableIterator::new(rows.collect::<Vec<_>>()))
    })
}

#[cfg(test)]
mod listing {
    use super::*;

    fn entry(
        kind: Kind,
        graph_id: i32,
        version: Option<VersionId>,
        vector_id: Option<i64>,
    ) -> Entry {
        Entry {
            kind,
            graph_id,
            version,
            vector: (kind == Kind::Transient).then(|| "t".to_owned()),
            vector_id,
            bytes: 10,
            state: State::Idle,
            last_use: Some(5),
        }
    }

    fn catalog() -> Catalog {
        let mut catalog = Catalog::default();
        catalog.graphs.insert(1, ("g".to_owned(), true));
        catalog.graphs.insert(2, ("secret".to_owned(), false));
        catalog.vectors.insert(7, (3, "rank".to_owned()));
        catalog
    }

    #[test]
    fn rows_list_what_the_caller_may_read_then_the_backends_total() {
        let entries = vec![
            entry(Kind::Transient, 1, Some(3), None),
            entry(Kind::Vector, 1, None, Some(7)),
            entry(Kind::Version, 1, Some(3), None),
            entry(Kind::Version, 2, Some(4), None),
            entry(Kind::Vector, 1, None, Some(8)),
            entry(Kind::Version, 9, Some(5), None),
        ];
        let rows = rows(entries, &catalog(), 1234, 4096, 99);
        let shown: Vec<_> =
            rows.iter().map(|row| (row.kind, row.version, row.vector.as_deref())).collect();
        assert_eq!(
            shown,
            [
                ("version", Some(3), None),
                ("vector", Some(3), Some("rank")),
                ("transient", Some(3), Some("t")),
                ("backend", None, None),
            ],
            "the unreadable graph's version, a vector whose row is gone and a graph that is \
             gone are not listed"
        );
        let total = rows.last().unwrap();
        assert_eq!((total.bytes, total.cap, total.last_use), (1234, Some(4096), Some(99)));
    }

    #[test]
    fn an_entrys_state_says_who_holds_it() {
        assert_eq!(State::of(false, true, true), State::Held);
        assert_eq!(State::of(true, true, false), State::InUse);
        assert_eq!(State::of(true, false, true), State::Pinned);
        assert_eq!(State::of(true, false, false), State::Idle);
        let names: Vec<_> =
            [State::InUse, State::Pinned, State::Idle, State::Held].map(State::name).into();
        assert_eq!(names, ["in use", "pinned", "idle", "held"]);
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::cache::end_statement;
    use crate::fixture::{refresh, ring, rows};

    /// `warren.cache()`'s rows, as `kind graph vector state`.
    fn listed() -> Result<Option<String>, Error> {
        rows(
            "SELECT format('%s:%s:%s:%s', kind, graph, vector, state) FROM warren.cache()
             WHERE kind = 'backend' OR graph = 'listed'",
        )
    }

    #[pg_test]
    fn the_cache_lists_its_versions_and_vectors_with_their_state() -> Result<(), Error> {
        ring("listed", 40, 1)?;
        refresh("listed", true)?;
        end_statement();
        Spi::run(
            "SELECT warren.vector_const('listed', 'kept', 1);
             SELECT warren.vector_const('listed', 'scratch', 2, persist := false)",
        )?;
        assert_eq!(
            listed()?.as_deref(),
            Some(
                "version:listed::pinned vector:listed:kept:pinned \
                 transient:listed:scratch:idle backend:::"
            ),
            "what the statement read is pinned"
        );
        end_statement();
        assert_eq!(
            listed()?.as_deref(),
            Some("version:listed::idle vector:listed:kept:idle transient:listed:scratch:idle backend:::")
        );
        let total = Spi::get_one::<bool>(
            "SELECT bytes = (SELECT sum(bytes) FROM warren.cache() WHERE kind <> 'backend')
                    AND cap = pg_size_bytes(current_setting('warren.max_cache_memory'))
             FROM warren.cache() WHERE kind = 'backend'",
        )?;
        assert_eq!(total, Some(true), "the backend row counts every entry against the cap");
        Ok(())
    }

    /// Like `shared_cache()`: no row of a graph the caller may not read,
    /// and never another role's transient vectors.
    #[pg_test]
    fn the_cache_hides_graphs_and_transients_the_caller_may_not_see() -> Result<(), Error> {
        ring("hidden", 40, 1)?;
        refresh("hidden", true)?;
        Spi::run(
            "SELECT warren.vector_const('hidden', 'mine', 2, persist := false);
             CREATE ROLE outsider",
        )?;
        let seen = Spi::get_one::<i64>(
            "SET LOCAL ROLE outsider;
             SELECT count(*) FROM warren.cache() WHERE kind <> 'backend'",
        )?;
        Spi::run("RESET ROLE")?;
        assert_eq!(seen, Some(0));
        Ok(())
    }
}
