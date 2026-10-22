//! `warren.last_stats()`: what the session's last recording traversal did
//! (DESIGN.md §9.3, Appendix A.6), and the `NOTICE` that makes truncation
//! visible. The traversals are `PARALLEL SAFE` (§14 item 33): one that
//! runs in a parallel worker records nothing, since the worker's record
//! could never reach the leader, so `last_stats()` describes only calls
//! the leader ran.

use std::cell::RefCell;

use pgrx::prelude::*;
use pgrx::JsonB;
use serde_json::{json, Value};
use warren_core::traversal::TraversalStats;

use crate::catalog::VersionId;

thread_local! {
    /// The stats of this session's last recording traversal.
    static LAST: RefCell<Option<Value>> = const { RefCell::new(None) };
}

/// `warren.last_stats()`: the stats of the session's last call of a
/// traversal function that records them (`bfs`, `reachable`,
/// `reachable_set`, `shortest_path`, `dijkstra`, `components`); NULL before
/// the first. Keys: `function`, `graph`, `version`, `nodes_visited`,
/// `edges_traversed`, `truncated`. A set-returning function updates them as
/// it streams, so after SQL `LIMIT` stops it they count the work done. A
/// call that a parallel worker ran is not recorded.
#[pg_extern(schema = "warren", volatile, parallel_restricted)]
fn last_stats() -> Option<JsonB> {
    LAST.with_borrow(|last| last.clone().map(JsonB))
}

/// Records the stats of one call as it runs.
pub(crate) struct Recorder {
    function: &'static str,
    graph: String,
    version: VersionId,
    noticed: bool,
}

impl Recorder {
    /// Starts recording a call of `function` on `version` of `graph`: until
    /// it records stats, `last_stats()` reports no work.
    pub(crate) fn start(function: &'static str, graph: &str, version: VersionId) -> Self {
        let recorder = Self { function, graph: graph.to_owned(), version, noticed: false };
        recorder.store(TraversalStats::default());
        recorder
    }

    /// Records the stats so far. The first time they say the scan was
    /// truncated, raises a `NOTICE` saying so.
    pub(crate) fn record(&mut self, stats: TraversalStats) {
        self.store(stats);
        if stats.truncated && !self.noticed {
            self.noticed = true;
            ereport!(
                NOTICE,
                PgSqlErrorCode::ERRCODE_SUCCESSFUL_COMPLETION,
                truncation_notice(self.function, &self.graph)
            );
        }
    }

    fn store(&self, stats: TraversalStats) {
        if in_parallel_worker() {
            return;
        }
        let value = stats_json(self.function, &self.graph, self.version, stats);
        LAST.with_borrow_mut(|last| *last = Some(value));
    }
}

/// Whether this process is a parallel worker (Postgres's
/// `IsParallelWorker()`).
fn in_parallel_worker() -> bool {
    // SAFETY: reads the process's parallel worker number, set once as a
    // parallel worker starts and -1 in every other backend, on the main
    // thread.
    unsafe { pg_sys::ParallelWorkerNumber >= 0 }
}

fn stats_json(function: &str, graph: &str, version: VersionId, stats: TraversalStats) -> Value {
    json!({
        "function": function,
        "graph": graph,
        "version": version,
        "nodes_visited": stats.nodes_visited,
        "edges_traversed": stats.edges_traversed,
        "truncated": stats.truncated,
    })
}

fn truncation_notice(function: &str, graph: &str) -> String {
    format!(
        "warren.{function} on graph \"{graph}\" stopped at max_results: the result is truncated"
    )
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use super::*;

    #[test]
    fn stats_become_json_with_the_call_they_describe() {
        let stats = TraversalStats { nodes_visited: 3, edges_traversed: 5, truncated: true };
        assert_eq!(
            stats_json("bfs", "kg", 7, stats),
            json!({"function": "bfs", "graph": "kg", "version": 7, "nodes_visited": 3,
                   "edges_traversed": 5, "truncated": true})
        );
        assert_eq!(
            truncation_notice("bfs", "kg"),
            "warren.bfs on graph \"kg\" stopped at max_results: the result is truncated"
        );
    }
}
