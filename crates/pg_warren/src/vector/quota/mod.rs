//! Quotas on readers' vectors (DESIGN.md §5.7, §14 items 46 and 47): what
//! each role other than the graph's owner may store as persisted vectors
//! on one graph, counted as their elements' bytes
//! (`length × width × size(dtype)`) across all its versions.
//!
//! The quota is `graphs.reader_vector_quota` (the default for every role)
//! or the role's row of `warren.vector_quotas`, set by the owner with
//! `warren.set_vector_quota` and shown by `warren.vector_usage`. A
//! persisting write is admitted by [`admit`]: members of the owner role
//! are exempt; otherwise it takes the (graph, role) lock, then reads the
//! quota and, where one applies, sums the role's usage under the latest
//! committed state, and refuses (53400) only a write that takes usage past
//! the quota *and* above what it was. The ledger (lock, quota, usage,
//! decision) is `catalog::quota`, which `reassign_owned` shares.
//! Transient vectors, `compact`'s move, drops and retention are not
//! counted.

mod enforce;
mod error;
mod sql;

pub(crate) use enforce::admit;
pub(crate) use error::QuotaError;
