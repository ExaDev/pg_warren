//! Eigenvalues of stored eigenvectors: the Rayleigh quotient `xᵀMx / xᵀx`
//! of each vector under a graph operator, which is its eigenvalue when the
//! vector is an eigenvector. The `spectral` distance uses it to compare
//! eigenvalues without solving again (DESIGN.md §8).

use super::operator::LinearOperator;
use super::{GraphMatrix, Resources, SpectralError};
use crate::compute::{AnyVector, ComputeError, Dtype, Element};
use crate::graph::GraphCsr;
use crate::interrupts::Interrupts;

/// The Rayleigh quotient of each of `vectors` (width-1, `f32` or `f64`,
/// over the graph's id space) under `matrix`. For the random walk, whose
/// eigenvectors `x` are those of `D^-1/2 S D^-1/2` scaled by `D^-1/2`, it is
/// taken of `D^1/2 x` under that symmetric matrix.
pub fn rayleigh_quotients<I: Interrupts>(
    resources: &mut Resources<'_, I>,
    graph: &GraphCsr,
    matrix: &GraphMatrix<'_>,
    vectors: &[&AnyVector],
) -> Result<Vec<f64>, SpectralError> {
    let mut operator = LinearOperator::new(resources, graph, matrix)?;
    let id_space = operator.id_space();
    let mut quotients = Vec::with_capacity(vectors.len());
    for vector in vectors {
        if vector.id_space() != id_space {
            return Err(ComputeError::IdSpaceMismatch {
                expected: id_space,
                found: vector.id_space(),
            }
            .into());
        }
        if vector.width() != 1 {
            return Err(ComputeError::WidthMismatch { expected: 1, found: vector.width() }.into());
        }
        let input = operator.input_mut()?;
        match vector {
            AnyVector::F32(v) => copy(v.values(), input),
            AnyVector::F64(v) => copy(v.values(), input),
            other => {
                let found = other.dtype();
                return Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found }.into());
            }
        }
        operator.unmap_eigenvector(resources)?;
        quotients.push(operator.rayleigh_quotient(resources)?);
    }
    Ok(quotients)
}

/// `out = values`, as `f64`.
fn copy<E: Element>(values: &[E], out: &mut [f64]) {
    for (out, value) in out.iter_mut().zip(values) {
        *out = value.to_f64();
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests {
    use super::*;
    use crate::compute::reference;
    use crate::compute::{ComputeError, ComputeMemory, Dtype, Vector, Workers};
    use crate::interrupts::Uninterrupted;
    use crate::spectral::{eigen, EigenOptions, Operator, Which};

    #[test]
    fn the_quotient_of_an_eigenvector_is_its_eigenvalue() {
        let edges: Vec<_> = (0..40u64)
            .flat_map(|i| {
                [(0, i, (i * 7 + 3) % 40, 1.0 + (i % 5) as f64), (0, i, (i + 1) % 40, 1.0)]
            })
            .collect();
        let graph = reference::graph(41, 1, &edges, true, true);
        let workers = Workers::new(2).unwrap();
        let memory = ComputeMemory::new(1 << 24);
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        for operator in Operator::ALL {
            for which in [Which::Largest, Which::Smallest] {
                let matrix = GraphMatrix::new(operator);
                let options = EigenOptions { which, ..EigenOptions::new(3) };
                let solved = eigen(&mut resources, &graph, &matrix, &options).unwrap();
                let vectors: Vec<_> = solved.pairs.iter().map(|p| &p.vector).collect();
                let quotients =
                    rayleigh_quotients(&mut resources, &graph, &matrix, &vectors).unwrap();
                for (pair, quotient) in solved.pairs.iter().zip(quotients) {
                    assert!(
                        (pair.value - quotient).abs() < 1e-5,
                        "{operator:?}: {} vs {quotient}",
                        pair.value
                    );
                }
            }
        }
    }

    #[test]
    fn vectors_must_cover_the_id_space() {
        let graph = reference::graph(3, 1, &vec![(0, 0, 1, 1.0)], false, true);
        let workers = Workers::new(1).unwrap();
        let memory = ComputeMemory::new(1 << 20);
        let mut resources =
            Resources { workers: &workers, memory: &memory, interrupts: &mut Uninterrupted };
        let short: AnyVector =
            Vector::from_values(&memory, 2, 1, vec![1.0f64, 0.0]).unwrap().into();
        let matrix = GraphMatrix::new(Operator::Adjacency);
        assert_eq!(
            rayleigh_quotients(&mut resources, &graph, &matrix, &[&short]),
            Err(ComputeError::IdSpaceMismatch { expected: 3, found: 2 }.into())
        );
        let ints: AnyVector = Vector::from_values(&memory, 3, 1, vec![1i64, 0, 0]).unwrap().into();
        assert_eq!(
            rayleigh_quotients(&mut resources, &graph, &matrix, &[&ints]),
            Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found: Dtype::I64 }.into())
        );
    }
}
