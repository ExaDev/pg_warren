//! The SPI adapter of the segment store: `warren.segments`, read and
//! written as the catalog owner (DESIGN.md §5.1, §7).

use pgrx::datum::DatumWithOid;
use pgrx::prelude::*;
use pgrx::varlena::varlena_to_byte_slice;
use pgrx::PgMemoryContexts;

use super::chunk::EachChunk;
use super::{SegmentError, SegmentStore};
use crate::access::as_catalog_owner;
use crate::catalog::VersionId;
use crate::read_only::read_one;

/// `warren.segments`.
pub(crate) struct SpiSegmentStore;

impl SegmentStore for SpiSegmentStore {
    fn write_chunk(
        &mut self,
        version: VersionId,
        segment: &str,
        chunk_no: i32,
        data: &[u8],
    ) -> Result<(), SegmentError> {
        as_catalog_owner(|| {
            Spi::connect_mut(|client| {
                client.update(
                    "INSERT INTO warren.segments (version_id, segment, chunk_no, data)
                     VALUES ($1, $2, $3, $4)",
                    None,
                    &[version.into(), segment.into(), chunk_no.into(), data.into()],
                )?;
                Ok(())
            })
        })
    }

    fn segment_bytes(
        &mut self,
        version: VersionId,
        segment: &str,
    ) -> Result<Option<u64>, SegmentError> {
        as_catalog_owner(|| {
            Ok(read_one::<i64>(
                "SELECT sum(octet_length(data))::bigint FROM warren.segments
                 WHERE version_id = $1 AND segment = $2",
                &[version.into(), segment.into()],
            )?
            .map(|bytes| bytes.unsigned_abs()))
        })
    }

    fn read_chunks(
        &mut self,
        version: VersionId,
        segment: &str,
        each: &mut EachChunk<'_, SegmentError>,
    ) -> Result<(), SegmentError> {
        as_catalog_owner(|| {
            each_chunk(
                "SELECT chunk_no, data FROM warren.segments
                 WHERE version_id = $1 AND segment = $2 ORDER BY chunk_no",
                &[version.into(), segment.into()],
                each,
            )
        })
    }
}

/// Runs `query` (rows of chunk number and bytes, in chunk order) through a
/// read-only cursor and passes each chunk to `each`, one row at a time.
/// A chunk stored out of line is detoasted into a memory context reset
/// after each row, and the row's tuple table is freed, so reading holds
/// about one chunk rather than every chunk until SPI finishes.
pub(crate) fn each_chunk<E: From<pgrx::spi::Error>>(
    query: &str,
    args: &[DatumWithOid<'_>],
    each: &mut EachChunk<'_, E>,
) -> Result<(), E> {
    Spi::connect(|client| {
        let mut cursor = client.try_open_cursor(query, args)?;
        let mut chunk = PgMemoryContexts::new("warren chunk");
        loop {
            let rows = cursor.fetch(1)?;
            if rows.is_empty() {
                return Ok(());
            }
            let row = rows.first();
            let chunk_no = row.get::<i32>(1)?.unwrap_or_default();
            let datum = row.get_datum_by_ordinal(2)?;
            // SAFETY: `chunk` is a context this function owns. The datum is
            // the bytea of the row just fetched; only its detoasted copy
            // (pgrx's typed getters would put it in the SPI procedure
            // context, which lives until SPI finishes) is allocated in
            // `chunk`, and it is used only by `each`, which copies what it
            // keeps, before the reset below. `each` itself runs in the
            // caller's context, so what it allocates outlives the row.
            let data = unsafe {
                chunk.switch_to(|_| match datum {
                    Some(datum) => {
                        varlena_to_byte_slice(pg_sys::pg_detoast_datum_packed(datum.cast_mut_ptr()))
                    }
                    None => &[],
                })
            };
            let passed = each(chunk_no, data);
            // SAFETY: nothing allocated in `chunk` is used after `each` returned.
            unsafe { chunk.reset() };
            passed?;
            // SAFETY: SPI_tuptable is the row just consumed, which nothing
            // refers to any more.
            unsafe { pg_sys::SPI_freetuptable(pg_sys::SPI_tuptable) };
        }
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::SpiSegmentStore;
    use crate::fixture::{refresh, ring};
    use crate::segment::{write_segment, SegmentStore};

    /// Bytes allocated in every Postgres memory context of the backend.
    fn allocated() -> usize {
        // SAFETY: TopMemoryContext is valid for the life of the backend.
        unsafe { pg_sys::MemoryContextMemAllocated(pg_sys::TopMemoryContext, true) }
    }

    /// A segment of 4 MiB in 64 KiB chunks (stored out of line, so each
    /// read detoasts a copy) is read holding about one chunk, not all.
    #[pg_test]
    fn reading_a_segment_holds_one_chunk_at_a_time() -> Result<(), Error> {
        ring("chunked", 10, 1)?;
        let version = refresh("chunked", true)?;
        let bytes = vec![7u8; 4 << 20];
        write_segment(&mut SpiSegmentStore, version, "probe", &bytes, 64 << 10)
            .map_err(|_| Error::InvalidPosition)?;
        let (before, mut peak, mut read) = (allocated(), 0, 0);
        SpiSegmentStore
            .read_chunks(version, "probe", &mut |_, data| {
                peak = peak.max(allocated().saturating_sub(before));
                read += data.len();
                Ok(())
            })
            .map_err(|_| Error::InvalidPosition)?;
        assert_eq!(read, bytes.len());
        assert!(peak < 1 << 20, "{peak} bytes allocated while reading");
        Ok(())
    }

    /// The name of the current memory context.
    fn current_context() -> String {
        // SAFETY: CurrentMemoryContext is always a valid context, and its
        // name a NUL-terminated string.
        unsafe { std::ffi::CStr::from_ptr((*pg_sys::CurrentMemoryContext).name) }
            .to_string_lossy()
            .into_owned()
    }

    /// What a visitor allocates must outlive the row: only the detoast
    /// runs in the context reset after each row.
    #[pg_test]
    fn a_visitor_runs_outside_the_per_chunk_context() -> Result<(), Error> {
        ring("visited", 10, 1)?;
        let version = refresh("visited", true)?;
        write_segment(&mut SpiSegmentStore, version, "probe", &[7u8; 4096], 1024)
            .map_err(|_| Error::InvalidPosition)?;
        let mut contexts = Vec::new();
        SpiSegmentStore
            .read_chunks(version, "probe", &mut |_, _| {
                contexts.push(current_context());
                Ok(())
            })
            .map_err(|_| Error::InvalidPosition)?;
        assert_eq!(contexts.len(), 4);
        assert!(contexts.iter().all(|name| name != "warren chunk"), "{contexts:?}");
        Ok(())
    }
}
