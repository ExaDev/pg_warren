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

/// Makes room for at least one more element, growing geometrically.
pub(crate) fn reserve_one<T>(buffer: &mut Vec<T>) -> Result<(), GraphError> {
    if buffer.len() < buffer.capacity() {
        return Ok(());
    }
    let additional = buffer.len().max(1024);
    buffer
        .try_reserve(additional)
        .map_err(|_| GraphError::OutOfMemory { elements: buffer.len() + additional })
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

    #[test]
    fn reserve_one_grows_a_full_buffer() {
        let mut buffer: Vec<u8> = Vec::new();
        assert_eq!(reserve_one(&mut buffer), Ok(()));
        assert!(buffer.capacity() > 0);
    }
}
