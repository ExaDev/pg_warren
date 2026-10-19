//! Traversal bounds (DESIGN.md §9.3); direction and relations are in
//! `selection`.

use super::TraversalSqlError;

/// A depth or count bound: `None` is unbounded; a negative one is an error.
pub(crate) fn bound<T: TryFrom<i64>>(
    name: &'static str,
    value: Option<i64>,
) -> Result<Option<T>, TraversalSqlError> {
    value.map(|value| T::try_from(value).map_err(|_| TraversalSqlError::Negative(name))).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_unbounded_when_null_and_never_negative() {
        assert_eq!(bound::<u32>("max_depth", None).ok(), Some(None));
        assert_eq!(bound::<u32>("max_depth", Some(3)).ok(), Some(Some(3)));
        assert!(matches!(
            bound::<u64>("max_results", Some(-1)),
            Err(TraversalSqlError::Negative("max_results"))
        ));
    }
}
