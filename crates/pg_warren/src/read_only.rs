//! Read-only statements: single-row SPI reads that never assign a
//! transaction id (DESIGN.md §6.9, §9.3).
//!
//! pgrx's `Spi::get_one*` and `get_two*` run their query through
//! `SpiClient::update`, which marks the transaction as writing by calling
//! `GetCurrentTransactionId()`: that assigns a transaction id, which
//! Postgres refuses in parallel mode ("cannot assign XIDs during a
//! parallel operation"). Every read on a path a query function takes goes
//! through here instead: `SpiClient::select` never calls `mark_mutable`, so
//! reading a graph never assigns a transaction id and works while a
//! parallel plan runs. It promises nothing about snapshots: as for any SPI
//! read, once the transaction has written, pgrx runs the select
//! non-read-only (a command counter increment and a fresh snapshot).
//! Depends on no other feature.

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use pgrx::spi::SpiResult;
use pgrx::{FromDatum, IntoDatum};

/// The first column of the first row `query` returns, read-only.
pub(crate) fn read_one<A: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<Option<A>> {
    Spi::connect(|client| client.select(query, Some(1), args)?.first().get_one())
}

/// The first two columns of the first row `query` returns, read-only.
pub(crate) fn read_two<A: FromDatum + IntoDatum, B: FromDatum + IntoDatum>(
    query: &str,
    args: &[DatumWithOid<'_>],
) -> SpiResult<(Option<A>, Option<B>)> {
    Spi::connect(|client| client.select(query, Some(1), args)?.first().get_two())
}
