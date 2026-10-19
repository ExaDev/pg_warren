//! Fallible allocation for graph-sized buffers: an allocation failure is a
//! [`GraphError::OutOfMemory`], never an abort (CLAUDE.md).

use super::GraphError;

/// An empty vector with room for `capacity` elements.
pub(crate) fn with_capacity<T>(capacity: usize) -> Result<Vec<T>, GraphError> {
    let mut buffer = Vec::new();
    buffer
        .try_reserve_exact(capacity)
        .map_err(|_| GraphError::OutOfMemory { elements: capacity })?;
    Ok(buffer)
}

/// A vector of `len` copies of `value`.
pub(crate) fn filled<T: Clone>(len: usize, value: T) -> Result<Vec<T>, GraphError> {
    let mut buffer = with_capacity(len)?;
    buffer.resize(len, value);
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_impossible_allocation_is_an_error_not_an_abort() {
        assert_eq!(
            with_capacity::<u64>(usize::MAX),
            Err(GraphError::OutOfMemory { elements: usize::MAX })
        );
    }

    #[test]
    fn filled_has_the_requested_length() {
        assert_eq!(filled(3, 7u8), Ok(vec![7, 7, 7]));
    }
}
