//! The eigenpairs the connected components give exactly, taken without
//! Lanczos, and Lanczos kept to the rest of the spectrum (DESIGN.md §6.5).
//!
//! Each connected component `C` of the active nodes gives the Laplacian
//! eigenvalue 0 once, with eigenvector `1_C`, the normalised Laplacian 0
//! with `D^1/2 1_C`, and the random walk 1 with right eigenvector `1_C`
//! (`D^1/2 1_C` of the symmetric matrix it is solved through). With several
//! components that eigenvalue repeats, and Lanczos from one start vector
//! finds one copy of it; the others come only from rounding, and the solve
//! spends its steps on the eigenvalues next to it instead (roadNet-TX's 424
//! components, DESIGN.md §12). Where it is at the wanted end (the
//! Laplacians' smallest, the random walk's largest), these pairs are
//! returned as they are, and Lanczos runs on their orthogonal complement:
//! its start vectors and every product are projected onto it. The
//! components' vectors have disjoint supports, so all of them fit in one
//! vector of the id space, and projecting against them is one pass however
//! many components there are.
//!
//! Without a negative entry in the matrix, both Laplacians are positive
//! semidefinite and the random walk's eigenvalues are at most 1 in
//! magnitude, so the components' pairs come first and Lanczos computes only
//! the rest of the `k`. With one, Lanczos computes `k` pairs of the
//! complement, merged with the components' in `which` order.

use super::components::Components;
use super::operator::LinearOperator;
use super::{dense, Operator, Resources, SpectralError, Which};
use crate::compute::MemoryCharge;
use crate::interrupts::Interrupts;

/// The components' eigenpairs of one operator.
#[derive(Debug)]
pub(super) struct Deflation {
    /// Their eigenvalue.
    value: f64,
    components: Components,
    /// At each node, its component's unit eigenvector's entry (0 at
    /// inactive nodes).
    unit: Vec<f64>,
    /// Each component's coefficient in the vector being projected.
    coefficients: Vec<f64>,
    /// Whether the matrix has no negative entry.
    nonnegative: bool,
    _charges: [MemoryCharge; 2],
}

impl Deflation {
    /// The eigenvalue every component gives once, if it is at the `which`
    /// end of `operator`'s spectrum (when the matrix has no negative entry).
    pub(super) fn eigenvalue(operator: Operator, which: Which) -> Option<f64> {
        match (operator, which) {
            (Operator::Laplacian | Operator::NormalizedLaplacian, Which::Smallest) => Some(0.0),
            (Operator::RandomWalk, Which::Largest) => Some(1.0),
            _ => None,
        }
    }

    /// The components' eigenpairs of `operator`, whose eigenvalue is `value`.
    pub(super) fn new<I: Interrupts>(
        resources: &mut Resources<'_, I>,
        operator: &mut LinearOperator<'_>,
        value: f64,
    ) -> Result<Self, SpectralError> {
        let nonnegative = operator.nonnegative(resources)?;
        let components = operator.components(resources)?;
        let component = components.of_nodes();
        let (mut unit, unit_charge) = resources.memory.filled(component.len(), 0.0)?;
        let (mut coefficients, coefficients_charge) =
            resources.memory.filled(components.count() as usize, 0.0)?;
        for (node, (entry, &c)) in unit.iter_mut().zip(component).enumerate() {
            *entry = operator.component_weight(node);
            coefficients[c as usize] += *entry * *entry;
        }
        for (entry, &c) in unit.iter_mut().zip(component) {
            if *entry != 0.0 {
                *entry /= coefficients[c as usize].sqrt();
            }
        }
        Ok(Self {
            value,
            components,
            unit,
            coefficients,
            nonnegative,
            _charges: [unit_charge, coefficients_charge],
        })
    }

    /// Their eigenvalue.
    pub(super) fn value(&self) -> f64 {
        self.value
    }

    /// The number of components.
    pub(super) fn count(&self) -> u64 {
        self.components.count()
    }

    /// How many of `k` pairs Lanczos computes, over `dimension` active
    /// nodes: those the components' do not give when they come first, else
    /// `k` (at most the dimension of the complement).
    pub(super) fn wanted(&self, k: usize, dimension: u64) -> usize {
        let count = usize::try_from(self.count()).unwrap_or(usize::MAX);
        match self.nonnegative {
            true => k.saturating_sub(count),
            false => {
                let complement = dimension.saturating_sub(self.count());
                let complement = usize::try_from(complement).unwrap_or(usize::MAX);
                k.min(complement)
            }
        }
    }

    /// Projects `x` onto the complement of the components' eigenvectors,
    /// `x − Σ u_C (u_C · x)`, and returns its norm. Each component's
    /// coefficient is summed in node order, so the result is the same for
    /// any thread count.
    pub(super) fn project<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        x: &mut [f64],
    ) -> Result<f64, SpectralError> {
        let (component, unit) = (self.components.of_nodes(), &self.unit);
        self.coefficients.fill(0.0);
        for ((&c, &u), &x) in component.iter().zip(unit).zip(x.iter()) {
            self.coefficients[c as usize] += u * x;
        }
        let coefficients = &self.coefficients;
        dense::update(resources, x, |i, v| v - unit[i] * coefficients[component[i] as usize])?;
        Ok(dense::sum(resources, x.len(), |i| x[i] * x[i])?.sqrt())
    }

    /// Writes the unit eigenvector of component `c` to `x`.
    pub(super) fn vector<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        c: u64,
        x: &mut [f64],
    ) -> Result<(), SpectralError> {
        let (component, unit) = (self.components.of_nodes(), &self.unit);
        dense::update(resources, x, |i, _| if component[i] == c { unit[i] } else { 0.0 })?;
        Ok(())
    }
}
