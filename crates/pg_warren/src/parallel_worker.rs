//! Parallel workers (DESIGN.md §9.3, §14 item 33): whether this process is
//! one, and what a worker leaves to its leader. The `PARALLEL SAFE` graph
//! reads run in workers, which Postgres starts per parallel plan: a
//! worker's `last_stats()` record could never reach the leader, and its
//! notices reach the client through the leader, so a notice about the
//! cache would arrive once per worker besides the leader's own. Depends
//! on no other feature.

use pgrx::pg_sys;

/// Whether this process is a parallel worker (Postgres's
/// `IsParallelWorker()`).
pub(crate) fn in_parallel_worker() -> bool {
    // SAFETY: reads the process's parallel worker number, set once as a
    // parallel worker starts and -1 in every other backend, on the main
    // thread.
    unsafe { pg_sys::ParallelWorkerNumber >= 0 }
}

/// Whether a `NOTICE` about the cache (a version over
/// `warren.max_cache_memory`, the shared cache refusing it or being
/// unavailable) that is `due` is raised in this process: never in a
/// parallel worker.
pub(crate) fn cache_notice_raised(due: bool) -> bool {
    raised(due, in_parallel_worker())
}

/// Whether a cache `NOTICE` that is `due` is raised in a process that is a
/// parallel `worker` or not. A worker's would repeat, once per worker, what
/// the leader says where it reads the version itself; a plan whose leader
/// makes no call (a single-copy `Gather`, as under `debug_parallel_query`)
/// says nothing.
fn raised(due: bool, worker: bool) -> bool {
    due && !worker
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cache_notice_is_raised_by_the_leader_never_by_a_worker() {
        assert!(raised(true, false), "due, in the leader or a backend");
        assert!(!raised(true, true), "due, in a parallel worker");
        assert!(!raised(false, false));
        assert!(!raised(false, true));
    }
}
