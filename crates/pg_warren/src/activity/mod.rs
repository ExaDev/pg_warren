//! Activity (DESIGN.md §5.7, §14 items 48 and 49): how other sessions see
//! a backend's cache and what it runs. A table in pg_warren's own shared memory, beside the
//! shared cache's registry (`placement`: a named DSM segment on PG 17 and
//! 18, the preload on PG 15 and 16), holds a slot per backend (`table`,
//! `slot`) that only its backend writes, without a lock, by a change-count
//! protocol (`change_count`); this backend keeps its own copy and claims
//! and gives back its slot (`backend`). The cache records its counters and
//! what it holds (`counters`); `warren.backends()` reads every slot
//! (`backends`), under `pg_stat_activity`'s privacy rule (`visibility`).
//! Each slot also holds two progress frames (`frame`): the operation, the
//! native call running now (`operation`), and the command around it, set
//! by `warren.report_progress` or a library function that reports as
//! it does (`command`); `warren.progress()` lists them
//! (`progress`).
//!
//! It depends on `access`, `shared_memory` and `catalog` (a report's
//! graph). `cache` and `vector` record into it, and the calls that report
//! progress, with `interrupts`, report to it.

mod backend;
mod backends;
mod change_count;
mod command;
mod counters;
mod frame;
mod operation;
mod placement;
mod progress;
mod slot;
mod table;
mod visibility;

pub(crate) use backend::claim;
#[cfg(test)]
pub(crate) use backend::own;
pub(crate) use command::{report, Report};
pub(crate) use counters::{
    count_private_evictions, count_read, count_refusal, count_shared_evictions, count_wait,
    record_mapped, record_private, Read,
};
pub(crate) use operation::{detail, done, phase, version, Running, Units};
pub(crate) use placement::request_preload;
pub(crate) use slot::{Memory, Refused};
