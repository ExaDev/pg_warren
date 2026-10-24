//! The eigenpairs the connected components give exactly, taken without
//! Lanczos, and Lanczos kept to the rest of the spectrum (DESIGN.md §6.5).
//!
//! Each connected component `C` of the active nodes gives the Laplacian
//! eigenvalue 0 once, with eigenvector `1_C`, the normalised Laplacian 0
//! with `D^1/2 1_C`, and the random walk 1 with right eigenvector `1_C`
//! (`D^1/2 1_C` of the symmetric matrix it is solved through). Each
//! bipartite one, with colouring `s_C` (±1 on its two sides), also gives
//! the random walk −1, with right eigenvector `s_C` (`D^1/2 s_C`), and the
//! normalised Laplacian 2 with `D^1/2 s_C`. With several components these
//! eigenvalues repeat, and Lanczos from one start vector finds one copy;
//! the others come only from rounding, and the solve spends its steps on
//! the eigenvalues next to them instead (roadNet-TX's 424 components,
//! DESIGN.md §12). Where they are at the wanted end (the Laplacians' 0 at
//! the smallest, the random walk's ±1 and the normalised Laplacian's 2 at
//! the largest), these pairs are returned as they are, and Lanczos runs on
//! their orthogonal complement: its start vectors and every product are
//! projected onto it. A component's vectors have its support, so all of
//! them fit in one vector of the id space and the colours, and projecting
//! against them is one pass however many components there are.
//!
//! Without a negative entry in the matrix, both Laplacians are positive
//! semidefinite, the normalised one's eigenvalues are at most 2 and the
//! random walk's at most 1 in magnitude, so the components' pairs come
//! first and Lanczos computes only the rest of the `k`. With one, Lanczos
//! computes `k` pairs of the complement, merged with the components' in
//! `which` order.

use super::components::Components;
use super::operator::LinearOperator;
use super::{dense, Operator, Resources, SpectralError, Which};
use crate::compute::MemoryCharge;
use crate::interrupts::Interrupts;

/// The eigenvalues the components give at one end of an operator's
/// spectrum.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Known {
    /// The eigenvalue every component gives once, if at that end.
    pub(super) connected: Option<f64>,
    /// The eigenvalue every bipartite component gives once, if at that end.
    pub(super) bipartite: Option<f64>,
}

/// One of the components' eigenpairs: its eigenvalue, the component, and
/// whether its vector is the bipartite one (alternating with the colours).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct KnownPair {
    pub(super) value: f64,
    component: u64,
    alternating: bool,
}

/// The components' eigenpairs of one operator.
#[derive(Debug)]
pub(super) struct Deflation {
    known: Known,
    components: Components,
    /// At each node, its component's unit eigenvector's entry for the
    /// connected eigenvalue (0 at inactive nodes); the bipartite vector's
    /// is this with the node's colour's sign.
    unit: Vec<f64>,
    /// Each component's coefficients in the vector being projected: along
    /// its connected vector and its bipartite one.
    coefficients: Vec<[f64; 2]>,
    /// Whether the matrix has no negative entry.
    nonnegative: bool,
    _charges: [MemoryCharge; 2],
}

impl Deflation {
    /// The eigenvalues the components give at the `which` end of
    /// `operator`'s spectrum (when the matrix has no negative entry), if
    /// any.
    pub(super) fn known(operator: Operator, which: Which) -> Option<Known> {
        let (connected, bipartite) = match (operator, which) {
            (Operator::Laplacian | Operator::NormalizedLaplacian, Which::Smallest) => {
                (Some(0.0), None)
            }
            (Operator::RandomWalk, Which::Largest) => (Some(1.0), Some(-1.0)),
            (Operator::NormalizedLaplacian, Which::Largest) => (None, Some(2.0)),
            _ => return None,
        };
        Some(Known { connected, bipartite })
    }

    /// The components' eigenpairs of `operator` with the `known`
    /// eigenvalues.
    pub(super) fn new<I: Interrupts>(
        resources: &mut Resources<'_, I>,
        operator: &mut LinearOperator<'_>,
        known: Known,
    ) -> Result<Self, SpectralError> {
        let nonnegative = operator.nonnegative(resources)?;
        let components = operator.components(resources)?;
        let component = components.of_nodes();
        let (mut unit, unit_charge) = resources.memory.filled(component.len(), 0.0)?;
        let (mut coefficients, coefficients_charge) =
            resources.memory.filled(components.count() as usize, [0.0; 2])?;
        for (node, (entry, &c)) in unit.iter_mut().zip(component).enumerate() {
            *entry = operator.component_weight(node);
            coefficients[c as usize][0] += *entry * *entry;
        }
        for (entry, &c) in unit.iter_mut().zip(component) {
            if *entry != 0.0 {
                *entry /= coefficients[c as usize][0].sqrt();
            }
        }
        Ok(Self {
            known,
            components,
            unit,
            coefficients,
            nonnegative,
            _charges: [unit_charge, coefficients_charge],
        })
    }

    /// Whether the matrix has no negative entry, so that nothing lies
    /// beyond the known eigenvalues at the wanted end.
    pub(super) fn nonnegative(&self) -> bool {
        self.nonnegative
    }

    /// The number of components.
    pub(super) fn components(&self) -> u64 {
        self.components.count()
    }

    /// The number of eigenpairs the components give: a connected one per
    /// component and a bipartite one per bipartite component, where wanted.
    pub(super) fn count(&self) -> u64 {
        let bipartite = self.components.bipartite().iter().filter(|&&b| b).count() as u64;
        let connected = self.known.connected.map_or(0, |_| self.components());
        connected + self.known.bipartite.map_or(0, |_| bipartite)
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

    /// The first `k` of the components' eigenpairs, in `which` order when
    /// the matrix has no negative entry: every component's connected pair,
    /// then every bipartite component's, each in component order.
    pub(super) fn pairs(&self, k: usize) -> impl Iterator<Item = KnownPair> + '_ {
        let count = self.components();
        let connected = self.known.connected.into_iter().flat_map(move |value| {
            (0..count).map(move |component| KnownPair { value, component, alternating: false })
        });
        let bipartite = self.known.bipartite.into_iter().flat_map(move |value| {
            (0..count)
                .filter(|&c| self.components.bipartite()[c as usize])
                .map(move |component| KnownPair { value, component, alternating: true })
        });
        connected.chain(bipartite).take(k)
    }

    /// Projects `x` onto the complement of the components' eigenvectors,
    /// `x − Σ v (v · x)`, and returns its norm. Each component's
    /// coefficients are summed in node order, so the result is the same for
    /// any thread count.
    pub(super) fn project<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        x: &mut [f64],
    ) -> Result<f64, SpectralError> {
        let (component, colour, unit) =
            (self.components.of_nodes(), self.components.colours(), &self.unit);
        self.coefficients.fill([0.0; 2]);
        for (((&c, &colour), &u), &x) in component.iter().zip(colour).zip(unit).zip(x.iter()) {
            let coefficients = &mut self.coefficients[c as usize];
            coefficients[0] += u * x;
            coefficients[1] += sign(colour) * u * x;
        }
        let (bipartite, coefficients) = (self.components.bipartite(), &self.coefficients);
        let connected = f64::from(u8::from(self.known.connected.is_some()));
        let alternating = f64::from(u8::from(self.known.bipartite.is_some()));
        dense::update(resources, x, |i, v| {
            let c = component[i] as usize;
            let along = connected * coefficients[c][0];
            let across = match bipartite[c] {
                true => alternating * sign(colour[i]) * coefficients[c][1],
                false => 0.0,
            };
            v - unit[i] * (along + across)
        })?;
        Ok(dense::sum(resources, x.len(), |i| x[i] * x[i])?.sqrt())
    }

    /// Writes the unit eigenvector of `pair` to `x`.
    pub(super) fn vector<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
        pair: KnownPair,
        x: &mut [f64],
    ) -> Result<(), SpectralError> {
        let (component, colour, unit) =
            (self.components.of_nodes(), self.components.colours(), &self.unit);
        dense::update(resources, x, |i, _| match component[i] == pair.component {
            true if pair.alternating => sign(colour[i]) * unit[i],
            true => unit[i],
            false => 0.0,
        })?;
        Ok(())
    }
}

/// `+1` for colour `false`, `−1` for `true`.
fn sign(colour: bool) -> f64 {
    match colour {
        false => 1.0,
        true => -1.0,
    }
}
