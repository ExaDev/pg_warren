//! A column: an expression's values over a run of elements, with NULL
//! flags. Values at inactive or NULL positions are placeholders.

use crate::compute::ComputeError;

/// The values of an expression of type `T` over a run of elements.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Column<T> {
    pub(super) values: Vec<T>,
    pub(super) nulls: Vec<bool>,
}

/// A column of any IR value type.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum AnyColumn {
    Bool(Column<bool>),
    I64(Column<i64>),
    F32(Column<f32>),
    F64(Column<f64>),
}

impl<T: Copy + Default> Column<T> {
    /// `len` copies of `value` (`None`: NULL).
    pub(super) fn constant(len: usize, value: Option<T>) -> Self {
        Self { values: vec![value.unwrap_or_default(); len], nulls: vec![value.is_none(); len] }
    }

    /// `f` of each active, non-NULL value; NULL stays NULL.
    pub(super) fn map<U: Default, E>(
        &self,
        active: &[bool],
        f: impl Fn(T) -> Result<U, E>,
    ) -> Result<Column<U>, ComputeError>
    where
        ComputeError: From<E>,
    {
        let values = (0..self.values.len())
            .map(|i| match active[i] && !self.nulls[i] {
                true => f(self.values[i]).map_err(ComputeError::from),
                false => Ok(U::default()),
            })
            .collect::<Result<_, _>>()?;
        Ok(Column { values, nulls: self.nulls.clone() })
    }

    /// `f` of each active pair of values; NULL where either is NULL.
    pub(super) fn zip<U: Copy, V: Default, E>(
        &self,
        other: &Column<U>,
        active: &[bool],
        f: impl Fn(T, U) -> Result<V, E>,
    ) -> Result<Column<V>, ComputeError>
    where
        ComputeError: From<E>,
    {
        let nulls: Vec<bool> = self.nulls.iter().zip(&other.nulls).map(|(a, b)| *a || *b).collect();
        let values = (0..nulls.len())
            .map(|i| match active[i] && !nulls[i] {
                true => f(self.values[i], other.values[i]).map_err(ComputeError::from),
                false => Ok(V::default()),
            })
            .collect::<Result<_, _>>()?;
        Ok(Column { values, nulls })
    }

    /// Copies `from` into the positions `which` selects.
    pub(super) fn assign(&mut self, from: &Self, which: &[bool]) {
        for (i, _) in which.iter().enumerate().filter(|(_, &set)| set) {
            self.values[i] = from.values[i];
            self.nulls[i] = from.nulls[i];
        }
    }
}
