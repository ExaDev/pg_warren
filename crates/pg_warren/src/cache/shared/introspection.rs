//! `warren.shared_cache()` (DESIGN.md §14 item 37): what the shared cache
//! holds of this database's graphs, one row per shared version or
//! persisted vector, and one row of the cluster's totals against the cap.
//!
//! Privacy: a version or vector is listed only if it belongs to this
//! database and installation of pg_warren, still exists as the statement's
//! snapshot sees it, and its graph is one the caller may read (a member of
//! its owner role or of a reader role), so no graph's name, version or
//! vector leaks to a role that cannot read it. The totals row counts every
//! slot of the cluster, of every database: only sizes and counts, which
//! any role can already infer from admission's `NOTICE`s and the cap.

use std::collections::HashMap;

use pgrx::pg_sys::Oid;
use pgrx::prelude::*;

use super::dsm::Dsm;
use super::registry::Listing;
use super::setting;
use super::slot::{Key, Kind, Slot, State};
use super::SharedVersions;
use crate::access::{as_catalog_owner, entry, is_reader};
use crate::cache::installation::installation;
use crate::cache::CacheError;
use crate::catalog::VersionId;
use crate::read_only::select;

/// What the catalog says a key names, as the statement's snapshot sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Named {
    /// The graph's name.
    pub(crate) graph: String,
    /// The version, or the version a vector is bound to.
    pub(crate) version: VersionId,
    /// A vector's name.
    pub(crate) vector: Option<String>,
    /// Whether the caller may read the graph.
    pub(crate) readable: bool,
}

/// One row of `warren.shared_cache()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    /// `version`, `vector`, or `cluster` for the totals.
    pub(crate) kind: &'static str,
    pub(crate) graph: Option<String>,
    pub(crate) version: Option<VersionId>,
    pub(crate) vector: Option<String>,
    pub(crate) vector_id: Option<i64>,
    /// `loading`, `ready` or `draining`; none for the totals.
    pub(crate) state: Option<&'static str>,
    pub(crate) bytes: i64,
    pub(crate) pins: i32,
    pub(crate) mappers: i32,
    /// The registry's clock at the slot's last pin; the clock now for the
    /// totals.
    pub(crate) last_use: i64,
    /// `warren.shared_cache_memory` in bytes, on the totals row.
    pub(crate) cap: Option<i64>,
}

/// The rows `listing` gives the caller in installation `here` (database and
/// extension oids): a row per slot of `here` that `named` names and the
/// caller may read, ordered by graph, version, kind and vector, then the
/// cluster's totals under `cap`.
pub(crate) fn rows(
    listing: &Listing,
    here: (Oid, Oid),
    cap: u64,
    named: impl Fn(&Key) -> Option<Named>,
) -> Vec<Row> {
    let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    let small = |value: u64| i32::try_from(value).unwrap_or(i32::MAX);
    let mut rows: Vec<Row> = listing
        .slots
        .iter()
        .filter(|(slot, _)| (slot.key.database, slot.key.extension) == here)
        .filter_map(|(slot, pins)| {
            let named = named(&slot.key).filter(|named| named.readable)?;
            let vector = slot.key.kind == Kind::Vector;
            Some(Row {
                kind: if vector { "vector" } else { "version" },
                graph: Some(named.graph),
                version: Some(named.version),
                vector: named.vector,
                vector_id: vector.then_some(slot.key.id),
                state: Some(state_name(slot.state)),
                bytes: count(slot.bytes),
                pins: small(u64::from(*pins)),
                mappers: small(u64::from(slot.mappers)),
                last_use: count(slot.last_used),
                cap: None,
            })
        })
        .collect();
    rows.sort_by(|a, b| {
        (&a.graph, a.version, a.kind, &a.vector).cmp(&(&b.graph, b.version, b.kind, &b.vector))
    });
    let sum = |each: fn(&(Slot, u32)) -> u64| -> u64 {
        listing.slots.iter().map(each).fold(0, u64::saturating_add)
    };
    rows.push(Row {
        kind: "cluster",
        graph: None,
        version: None,
        vector: None,
        vector_id: None,
        state: None,
        bytes: count(sum(|(slot, _)| slot.bytes)),
        pins: small(sum(|(_, pins)| u64::from(*pins))),
        mappers: small(sum(|(slot, _)| u64::from(slot.mappers))),
        last_use: count(listing.clock),
        cap: Some(count(cap)),
    });
    rows
}

/// A slot's state as the view names it.
fn state_name(state: State) -> &'static str {
    match state {
        State::Empty => "empty",
        State::Loading => "loading",
        State::Ready => "ready",
        State::Draining => "draining",
    }
}

/// `warren.shared_cache()`: the shared versions and persisted vectors of
/// this database's graphs that the caller may read, each with its state,
/// bytes, pins (statements pinning it), mappers (backends mapping it) and
/// last use (the registry's clock at its last pin), then one `cluster` row:
/// the bytes, pins and mappers of every slot in the cluster, the clock
/// now, and the cap (DESIGN.md §14 item 37). `PARALLEL SAFE`: it reads no
/// graph. With the shared cache unavailable, or off in a backend that has
/// not used it, only the `cluster` row, with nothing held.
#[pg_extern(schema = "warren", volatile, parallel_safe)]
// pgrx reads the columns' names from the return type itself, so they
// cannot be factored into a type alias.
#[allow(clippy::type_complexity)]
fn shared_cache() -> TableIterator<
    'static,
    (
        name!(kind, String),
        name!(graph, Option<String>),
        name!(version, Option<i64>),
        name!(vector, Option<String>),
        name!(vector_id, Option<i64>),
        name!(state, Option<String>),
        name!(bytes, i64),
        name!(pins, i32),
        name!(mappers, i32),
        name!(last_use, i64),
        name!(cap, Option<i64>),
    ),
> {
    entry(|_| {
        let rows = current_rows()?.into_iter().map(|row| {
            (
                row.kind.to_owned(),
                row.graph,
                row.version,
                row.vector,
                row.vector_id,
                row.state.map(str::to_owned),
                row.bytes,
                row.pins,
                row.mappers,
                row.last_use,
                row.cap,
            )
        });
        Ok::<_, CacheError>(TableIterator::new(rows))
    })
}

/// The rows of `warren.shared_cache()` now.
fn current_rows() -> Result<Vec<Row>, CacheError> {
    let cap = setting::cap();
    // At a cap of 0 the registry is looked at only by a backend that has
    // used it: on PG 17 and 18 looking would create it.
    let port = if cap > 0 { Dsm::open().ok() } else { Dsm::used() };
    let listing = port.map(|mut port| port.listing()).unwrap_or_default();
    let Some(here) = installation() else {
        return Ok(rows(&listing, (Oid::INVALID, Oid::INVALID), cap, |_| None));
    };
    let ours: Vec<Key> = listing
        .slots
        .iter()
        .map(|(slot, _)| slot.key)
        .filter(|key| (key.database, key.extension) == here)
        .collect();
    let names = names(&ours)?;
    Ok(rows(&listing, here, cap, |key| names.get(key).cloned()))
}

/// The graph, version and vector each of `keys` names as the statement's
/// snapshot sees them, and whether the caller may read the graph; keys
/// whose rows are gone are left out.
fn names(keys: &[Key]) -> Result<HashMap<Key, Named>, CacheError> {
    let ids = |kind: Kind| -> Vec<i64> {
        keys.iter().filter(|key| key.kind == kind).map(|key| key.id).collect()
    };
    let mut found = Vec::new();
    for (kind, query) in [(Kind::Version, VERSIONS), (Kind::Vector, VECTORS)] {
        let ids = ids(kind);
        if !ids.is_empty() {
            found.extend(catalog_rows(query, ids)?.into_iter().map(|row| (kind, row)));
        }
    }
    let mut readable: HashMap<String, bool> = HashMap::new();
    let mut names = HashMap::new();
    for (kind, row) in found {
        let may_read = match readable.get(&row.graph) {
            Some(&may_read) => may_read,
            None => {
                let may_read = is_reader(row.owner, &row.readers);
                readable.insert(row.graph.clone(), may_read);
                may_read
            }
        };
        let key = keys.iter().find(|key| key.kind == kind && key.id == row.id).copied();
        if let Some(key) = key {
            let named = Named {
                graph: row.graph,
                version: row.version,
                vector: row.vector,
                readable: may_read,
            };
            names.insert(key, named);
        }
    }
    Ok(names)
}

/// The versions among `$1`, with their graphs.
const VERSIONS: &str = "SELECT v.version_id, v.version_id, NULL::text, g.name, g.owner::oid,
                               g.readers::oid[]
                        FROM warren.versions AS v JOIN warren.graphs AS g USING (graph_id)
                        WHERE v.version_id = ANY ($1)";

/// The vectors among `$1`, with their versions and graphs.
const VECTORS: &str = "SELECT x.vector_id, x.version_id, x.name, g.name, g.owner::oid,
                              g.readers::oid[]
                       FROM warren.vectors AS x JOIN warren.graphs AS g USING (graph_id)
                       WHERE x.vector_id = ANY ($1)";

/// A version or vector as the catalog names it.
struct CatalogRow {
    id: i64,
    version: VersionId,
    vector: Option<String>,
    graph: String,
    owner: Oid,
    readers: Vec<Oid>,
}

/// The rows `query` reads for `ids`, as the catalog owner, under the
/// statement's snapshot, read-only.
fn catalog_rows(query: &str, ids: Vec<i64>) -> Result<Vec<CatalogRow>, CacheError> {
    as_catalog_owner(|| {
        Spi::connect(|client| {
            select(client, query, None, &[ids.into()], |rows| {
                rows.map(|row| {
                    Ok(CatalogRow {
                        id: row.get(1)?.unwrap_or_default(),
                        version: row.get(2)?.unwrap_or_default(),
                        vector: row.get(3)?,
                        graph: row.get(4)?.unwrap_or_default(),
                        owner: row.get(5)?.unwrap_or(Oid::INVALID),
                        readers: row.get(6)?.unwrap_or_default(),
                    })
                })
                .collect::<Result<Vec<_>, CacheError>>()
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::cache::shared::memory::{Backend, Memory};
    use crate::cache::shared::{Claimed, Found};

    const HERE: (Oid, Oid) = (Oid::INVALID, Oid::INVALID);

    fn here(kind: Kind, id: i64) -> Key {
        Key { database: HERE.0, extension: HERE.1, kind, id }
    }

    /// Claims and publishes `key`'s version or vector of `bytes` in
    /// `backend`, which keeps it pinned and mapped.
    fn published(backend: &mut Backend, key: Key, bytes: u64) {
        let Claimed::Load(region) = backend.claim(key, bytes) else {
            panic!("not claimed");
        };
        drop(backend.publish(region).unwrap());
    }

    /// The catalog of the tests: version 1 and its vector 1 of graph `g`,
    /// which the caller may read, and version 2 of graph `h`, which it may
    /// not; version 3 is gone.
    fn named(key: &Key) -> Option<Named> {
        let named = |graph: &str, version, vector: Option<&str>, readable| Named {
            graph: graph.to_owned(),
            version,
            vector: vector.map(str::to_owned),
            readable,
        };
        match (key.kind, key.id) {
            (Kind::Version, 1) => Some(named("g", 1, None, true)),
            (Kind::Vector, 1) => Some(named("g", 1, Some("x"), true)),
            (Kind::Version, 2) => Some(named("h", 2, None, false)),
            _ => None,
        }
    }

    #[test]
    fn rows_list_what_the_caller_may_read_and_the_clusters_totals() {
        let memory = Memory::new(8, 1000, u64::MAX, Duration::ZERO);
        let (mut loader, mut reader) = (memory.backend(), memory.backend());
        let elsewhere = Key { database: Oid::from(9), ..here(Kind::Version, 1) };
        let reinstalled = Key { extension: Oid::from(9), ..here(Kind::Version, 1) };
        for (key, bytes) in [
            (here(Kind::Vector, 1), 10),
            (here(Kind::Version, 1), 100),
            (here(Kind::Version, 2), 200),
            (here(Kind::Version, 3), 300),
            (elsewhere, 50),
            (reinstalled, 60),
        ] {
            published(&mut loader, key, bytes);
        }
        loader.unpin();
        let Found::Ready(view) = reader.lookup(here(Kind::Version, 1)) else {
            panic!("not found");
        };
        let listing = reader.listing();
        let rows = rows(&listing, HERE, 1000, named);
        let entries: Vec<_> = rows
            .iter()
            .map(|row| {
                let (graph, vector) = (row.graph.as_deref(), row.vector.as_deref());
                (row.kind, graph, row.version, vector, row.vector_id, row.state, row.pins)
            })
            .collect();
        assert_eq!(
            entries,
            [
                ("vector", Some("g"), Some(1), Some("x"), Some(1), Some("ready"), 0),
                ("version", Some("g"), Some(1), None, None, Some("ready"), 1),
                ("cluster", None, None, None, None, None, 1),
            ],
            "h's version, a version that is gone, another database's and another \
             installation's are not listed"
        );
        let (vector, version, cluster) = (&rows[0], &rows[1], &rows[2]);
        assert_eq!((vector.bytes, vector.mappers), (10, 1));
        assert_eq!((version.bytes, version.mappers), (100, 2));
        assert!(version.last_use > vector.last_use, "the reader's pin is the latest use");
        assert_eq!(cluster.bytes, 720, "the totals count every slot of the cluster");
        assert_eq!((cluster.mappers, cluster.cap), (7, Some(1000)));
        assert_eq!(cluster.last_use, i64::try_from(listing.clock).unwrap());
        assert_eq!(rows.iter().filter(|row| row.cap.is_some()).count(), 1);
        drop(view);
    }

    #[test]
    fn with_nothing_shared_only_the_totals_are_listed() {
        let rows = rows(&Listing::default(), HERE, 0, |_| panic!("named"));
        assert_eq!(rows.len(), 1);
        assert_eq!((rows[0].kind, rows[0].bytes, rows[0].cap), ("cluster", 0, Some(0)));
    }
}
