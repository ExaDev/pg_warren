//! The SPI adapter of the segment store: `warren.segments`, read and
//! written as the extension owner (DESIGN.md §5.1, §7).

use pgrx::prelude::*;

use super::{SegmentError, SegmentStore};
use crate::access::as_extension_owner;
use crate::catalog::VersionId;

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
        as_extension_owner(|| {
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

    fn read_chunks(
        &mut self,
        version: VersionId,
        segment: &str,
    ) -> Result<Vec<(i32, Vec<u8>)>, SegmentError> {
        as_extension_owner(|| {
            Spi::connect(|client| {
                client
                    .select(
                        "SELECT chunk_no, data FROM warren.segments
                         WHERE version_id = $1 AND segment = $2 ORDER BY chunk_no",
                        None,
                        &[version.into(), segment.into()],
                    )?
                    .map(|row| {
                        Ok((
                            row.get::<i32>(1)?.unwrap_or_default(),
                            row.get::<Vec<u8>>(2)?.unwrap_or_default(),
                        ))
                    })
                    .collect()
            })
        })
    }
}
