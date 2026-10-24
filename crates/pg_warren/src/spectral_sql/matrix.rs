//! The arguments that choose a graph operator and how to solve it, shared
//! by `eigen` and `distance(method := 'spectral')`.

use warren_core::graph::{RelationDictionary, RelationIndex, RelationSelection};
use warren_core::spectral::{EigenOptions, GraphMatrix};

use super::SpectralSqlError;
use crate::selection::relations;

/// The operator arguments of a call, before the relations are looked up in
/// a version's dictionary.
#[derive(Debug, Clone)]
pub(super) struct OperatorArguments {
    pub(super) matrix: String,
    pub(super) rels: Option<Vec<Option<String>>>,
    pub(super) weighted: Option<bool>,
    pub(super) symmetrize: bool,
    pub(super) per_relation: bool,
}

impl OperatorArguments {
    /// The relation indexes `rels` names in `dictionary` (all if `None`).
    pub(super) fn relations(&self, dictionary: &RelationDictionary) -> Option<Vec<RelationIndex>> {
        relations(dictionary, self.rels.as_deref())
    }

    /// The operator over `selected` relations (from [`Self::relations`]).
    pub(super) fn matrix<'a>(
        &self,
        selected: Option<&'a [RelationIndex]>,
    ) -> Result<GraphMatrix<'a>, SpectralSqlError> {
        Ok(GraphMatrix {
            operator: self.matrix.parse()?,
            relations: selected.map_or(RelationSelection::All, RelationSelection::Only),
            weighted: self.weighted,
            symmetrize: self.symmetrize,
            per_relation: self.per_relation,
        })
    }
}

/// The solver options of a call, checked.
pub(super) fn solver_options(
    k: Option<i32>,
    which: &str,
    tol: f64,
    max_iter: Option<i32>,
    dtype: &str,
) -> Result<EigenOptions, SpectralSqlError> {
    let k = k.ok_or(SpectralSqlError::NullArgument("k"))?;
    let k = usize::try_from(k)
        .ok()
        .filter(|&k| k > 0)
        .ok_or_else(|| SpectralSqlError::InvalidArgument(format!("k must be positive, not {k}")))?;
    if !(tol.is_finite() && tol >= 0.0) {
        return Err(SpectralSqlError::InvalidArgument(format!(
            "tol must be a non-negative number, not {tol}"
        )));
    }
    let max_iter = max_iter
        .map(|steps| {
            usize::try_from(steps).ok().filter(|&steps| steps > 0).ok_or_else(|| {
                SpectralSqlError::InvalidArgument(format!("max_iter must be positive, not {steps}"))
            })
        })
        .transpose()?;
    Ok(EigenOptions {
        which: which.parse()?,
        tol,
        max_iter,
        precision: dtype.parse()?,
        ..EigenOptions::new(k)
    })
}

#[cfg(test)]
mod unit {
    use super::*;
    use warren_core::spectral::{Precision, Which};

    #[test]
    fn options_are_checked_and_parsed() {
        let options = solver_options(Some(3), "smallest", 1e-6, Some(50), "f64").unwrap();
        assert_eq!((options.k, options.which, options.max_iter), (3, Which::Smallest, Some(50)));
        assert_eq!((options.tol, options.precision), (1e-6, Precision::F64));
        // The messages, not the error's Display: through ComputeSqlError it
        // can name SQL types, which needs a backend.
        let invalid = |k, tol, max_iter| match solver_options(k, "largest", tol, max_iter, "f32") {
            Err(SpectralSqlError::NullArgument(argument)) => format!("{argument} must not be NULL"),
            Err(SpectralSqlError::InvalidArgument(message)) => message,
            other => panic!("{other:?}"),
        };
        assert_eq!(invalid(None, 0.0, None), "k must not be NULL");
        assert_eq!(invalid(Some(0), 0.0, None), "k must be positive, not 0");
        assert_eq!(invalid(Some(1), -1.0, None), "tol must be a non-negative number, not -1");
        assert_eq!(invalid(Some(1), 0.0, Some(0)), "max_iter must be positive, not 0");
        assert!(solver_options(Some(1), "middle", 0.0, None, "f32").is_err());
        assert!(solver_options(Some(1), "largest", 0.0, None, "i64").is_err());
    }
}
