//! The symmetric matrices of a graph that an eigenproblem runs over,
//! applied matrix-free through SpMV (DESIGN.md §6.5, `matrix`).
//!
//! `S` is the adjacency matrix of the selected relations (weights summed
//! over relations, or 1 when unweighted; per relation, 1 in each), symmetrised as `(A + Aᵀ)/2` from
//! the forward and reverse CSRs, or taken as is when the caller says it is
//! already symmetric (checked). When every selected relation's reverse CSR
//! equals its forward CSR (an undirected graph's do), `A = Aᵀ` exactly and
//! symmetrising multiplies once. `D` is the diagonal of its row sums, the
//! weighted degrees; a self-loop counts once.
//!
//! The operators act on the **active** nodes: nodes with at least one edge
//! of `S`. Tombstoned nodes have no edges (DESIGN.md §5.2), and isolated
//! live nodes are left out too, so neither contributes a spurious
//! eigenvalue (each would add an exact 0 to the Laplacians, a whole
//! eigenspace of them when many nodes are isolated). Every vector the
//! operator is applied to, and so every eigenvector, is zero at inactive
//! nodes, and the operator maps such vectors to such vectors exactly.

use std::str::FromStr;

use super::components::Components;
use super::{dense, Resources, SpectralError};
use crate::compute::random;
use crate::compute::{
    spmv, AnyVector, ComputeError, Dtype, MemoryCharge, Product, Semiring, Vector,
};
use crate::graph::{GraphCsr, RelationSelection};
use crate::interrupts::Interrupts;

/// Which matrix of the graph an eigenproblem is over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operator {
    /// `S`.
    Adjacency,
    /// `L = D − S`.
    Laplacian,
    /// `I − D^-1/2 S D^-1/2`.
    NormalizedLaplacian,
    /// `P = D^-1 S`. It is not symmetric, so the eigenproblem runs over the
    /// similar matrix `D^-1/2 S D^-1/2` (same eigenvalues) and maps each
    /// eigenvector `φ` to the right eigenvector `D^-1/2 φ` of `P`.
    RandomWalk,
}

impl Operator {
    pub(crate) const ALL: [Self; 4] =
        [Self::Adjacency, Self::Laplacian, Self::NormalizedLaplacian, Self::RandomWalk];

    /// The name the SQL `matrix` argument uses.
    pub fn name(self) -> &'static str {
        match self {
            Self::Adjacency => "adjacency",
            Self::Laplacian => "laplacian",
            Self::NormalizedLaplacian => "normalized_laplacian",
            Self::RandomWalk => "random_walk",
        }
    }

    /// Whether the operator scales by `D^-1/2`.
    pub(super) fn normalized(self) -> bool {
        matches!(self, Self::NormalizedLaplacian | Self::RandomWalk)
    }
}

impl FromStr for Operator {
    type Err = SpectralError;

    fn from_str(name: &str) -> Result<Self, SpectralError> {
        Self::ALL
            .into_iter()
            .find(|operator| operator.name() == name)
            .ok_or_else(|| SpectralError::UnknownOperator(name.to_owned()))
    }
}

/// An operator over a graph: which matrix, of which relations' edges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GraphMatrix<'a> {
    /// The matrix.
    pub operator: Operator,
    /// The relations whose edges make up `A`.
    pub relations: RelationSelection<'a>,
    /// Use edge weights (`None`: if the graph has them).
    pub weighted: Option<bool>,
    /// Use `(A + Aᵀ)/2`. Without it, `A` must already be symmetric (an
    /// undirected graph), which saves an SpMV per step.
    pub symmetrize: bool,
    /// Read a pair linked in several relations as an edge in each, so an
    /// unweighted one weighs the number of its relations in `A` (DESIGN.md
    /// §5.3); by default it is one edge.
    pub per_relation: bool,
}

impl GraphMatrix<'_> {
    /// `operator` over every relation, weighted if the graph is, symmetrised.
    pub fn new(operator: Operator) -> Self {
        Self {
            operator,
            relations: RelationSelection::All,
            weighted: None,
            symmetrize: true,
            per_relation: false,
        }
    }
}

/// The seed of the random vector the symmetry check multiplies.
const SYMMETRY_SEED: u64 = 0x5359_4d4d;

/// How far `Ax` and `Aᵀx` may differ, relative to their size, for `A` to
/// count as symmetric.
const SYMMETRY_TOLERANCE: f64 = 1e-9;

/// An operator ready to apply: degrees computed, buffers charged.
#[derive(Debug)]
pub(super) struct LinearOperator<'g> {
    graph: &'g GraphCsr,
    operator: Operator,
    forward: Product<'g>,
    /// Whether an application adds the transposed product, `(A + Aᵀ)/2`:
    /// when symmetrising, unless `A = Aᵀ` exactly.
    transpose: bool,
    /// Whether each node has an edge.
    active: Vec<bool>,
    dimension: u64,
    /// The weighted degrees (`S·1`).
    degrees: Vec<f64>,
    /// `d^-1/2` at active nodes and 0 elsewhere, for the normalised
    /// operators; empty otherwise.
    scale: Vec<f64>,
    /// The buffers of one application, all `f64`.
    buffers: Buffers,
    _charges: [MemoryCharge; 3],
}

impl<'g> LinearOperator<'g> {
    /// Prepares `matrix` over `graph`.
    pub(super) fn new<I: Interrupts>(
        resources: &mut Resources<'_, I>,
        graph: &'g GraphCsr,
        matrix: &GraphMatrix<'g>,
    ) -> Result<Self, SpectralError> {
        let id_space = graph.all_relations().forward().id_space();
        let n = id_space as usize;
        let buffers = Buffers::new(resources, id_space, matrix.operator.normalized())?;
        let (active, active_charge) = resources.memory.filled(n, false)?;
        let (degrees, degrees_charge) = resources.memory.filled(n, 0.0)?;
        let scale_len = if matrix.operator.normalized() { n } else { 0 };
        let (scale, scale_charge) = resources.memory.filled(scale_len, 0.0)?;
        let forward = Product {
            relations: matrix.relations,
            weighted: matrix.weighted,
            per_relation: matrix.per_relation,
            ..Product::new(Semiring::PlusTimes)
        };
        let mut operator = Self {
            graph,
            operator: matrix.operator,
            forward,
            transpose: matrix.symmetrize
                && !reverses_are_forward(graph, matrix.relations, matrix.per_relation),
            active,
            dimension: 0,
            degrees,
            scale,
            buffers,
            _charges: [active_charge, degrees_charge, scale_charge],
        };
        if !matrix.symmetrize {
            operator.check_symmetric(resources)?;
        }
        operator.measure_degrees(resources)?;
        Ok(operator)
    }

    /// The number of rows of every vector the operator applies to.
    pub(super) fn id_space(&self) -> u64 {
        self.graph.all_relations().forward().id_space()
    }

    /// The number of active nodes: the dimension of the eigenproblem.
    pub(super) fn dimension(&self) -> u64 {
        self.dimension
    }

    /// The connected components of the active nodes.
    pub(super) fn components<I: Interrupts>(
        &self,
        resources: &mut Resources<'_, I>,
    ) -> Result<Components, SpectralError> {
        Components::of(resources, self.graph, self.forward.relations, &self.active)
    }

    /// The entry at `node` of the eigenvector its connected component
    /// gives (eigenvalue 0 of the Laplacians, 1 of the random walk), before
    /// normalising: `√d` for the operators that scale by `D^-1/2`, else 1;
    /// 0 at inactive nodes.
    pub(super) fn component_weight(&self, node: usize) -> f64 {
        match (self.active[node], self.operator.normalized()) {
            (false, _) => 0.0,
            (true, true) => self.degrees[node].sqrt(),
            (true, false) => 1.0,
        }
    }

    /// Whether `A` has no negative (or NaN) entry, so that `S` has none: then
    /// both Laplacians are positive semidefinite and the random walk's
    /// eigenvalues are at most 1 in magnitude. A negative self-loop counts,
    /// although the Laplacians cancel it.
    pub(super) fn nonnegative<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<bool, SpectralError> {
        let smallest = Product { semiring: Semiring::MinFirst, ..self.forward };
        let (graph, buffers) = (self.graph, &mut self.buffers);
        let Resources { workers, memory, interrupts } = &mut *resources;
        spmv(workers, memory, *interrupts, graph, &smallest, &buffers.input, &mut buffers.output)?;
        let y = values(&buffers.output)?;
        let negative = |v: f64| v.is_nan() || v < 0.0;
        let negatives = dense::max(resources, y.len(), |i| f64::from(u8::from(negative(y[i]))))?;
        Ok(negatives == 0.0)
    }

    /// The vector [`Self::apply`] multiplies, to be filled by the caller.
    pub(super) fn input_mut(&mut self) -> Result<&mut [f64], SpectralError> {
        values_mut(&mut self.buffers.input)
    }

    /// The output of the last [`Self::apply`], mutably.
    pub(super) fn output_mut(&mut self) -> Result<&mut [f64], SpectralError> {
        values_mut(&mut self.buffers.output)
    }

    /// The operator times the input vector.
    pub(super) fn apply<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<&mut [f64], SpectralError> {
        let (operator, forward) = (self.operator, self.forward);
        let buffers = &mut self.buffers;
        let (degrees, scale) = (&self.degrees, &self.scale);
        let input = values(&buffers.input)?;
        if operator.normalized() {
            dense::update(resources, values_mut(&mut buffers.scaled)?, |i, _| scale[i] * input[i])?;
        }
        let source = if operator.normalized() { Source::Scaled } else { Source::Input };
        buffers.multiply(resources, self.graph, forward, self.transpose, source)?;
        let input = values(&buffers.input)?;
        let output = values_mut(&mut buffers.output)?;
        match operator {
            Operator::Adjacency => {}
            Operator::Laplacian => {
                dense::update(resources, output, |i, y| degrees[i] * input[i] - y)?;
            }
            Operator::NormalizedLaplacian => {
                dense::update(resources, output, |i, y| input[i] - scale[i] * y)?;
            }
            Operator::RandomWalk => dense::update(resources, output, |i, y| scale[i] * y)?,
        }
        Ok(output)
    }

    /// `‖M·input − value·input‖₂`.
    pub(super) fn residual<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        value: f64,
    ) -> Result<f64, SpectralError> {
        self.apply(resources)?;
        let (x, y) = (values(&self.buffers.input)?, values(&self.buffers.output)?);
        Ok(dense::sum(resources, x.len(), |i| (y[i] - value * x[i]).powi(2))?.sqrt())
    }

    /// Fills the input with seeded uniform values at active nodes (zero
    /// elsewhere) and returns it.
    pub(super) fn random_input<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        seed: u64,
        stream: u64,
    ) -> Result<&mut [f64], SpectralError> {
        let active = &self.active;
        let input = values_mut(&mut self.buffers.input)?;
        dense::update(resources, input, |i, _| match active[i] {
            true => random::uniform(seed, stream, i as u64),
            false => 0.0,
        })?;
        Ok(input)
    }

    /// Maps the eigenvector `φ` in the input, in place, to the eigenvector
    /// of the matrix the operator stands for: `D^-1/2 φ` for the random
    /// walk, else `φ`. The result is not normalised. Returns whether the
    /// input changed.
    pub(super) fn map_eigenvector<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<bool, SpectralError> {
        if self.operator != Operator::RandomWalk {
            return Ok(false);
        }
        let scale = &self.scale;
        dense::update(resources, values_mut(&mut self.buffers.input)?, |i, x| scale[i] * x)?;
        Ok(true)
    }

    /// The inverse of [`Self::map_eigenvector`], in place on the input:
    /// `D^1/2 x` for the random walk (zero at inactive nodes), else `x`.
    pub(super) fn unmap_eigenvector<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<(), SpectralError> {
        if self.operator == Operator::RandomWalk {
            let scale = &self.scale;
            let unscaled = |i: usize, x: f64| if scale[i] == 0.0 { 0.0 } else { x / scale[i] };
            dense::update(resources, values_mut(&mut self.buffers.input)?, unscaled)?;
        }
        Ok(())
    }

    /// `xᵀMx / xᵀx` for the input `x`.
    pub(super) fn rayleigh_quotient<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<f64, SpectralError> {
        self.apply(resources)?;
        let (x, y) = (values(&self.buffers.input)?, values(&self.buffers.output)?);
        let numerator = dense::sum(resources, x.len(), |i| x[i] * y[i])?;
        let denominator = dense::sum(resources, x.len(), |i| x[i] * x[i])?;
        Ok(numerator / denominator)
    }

    /// Finds the active nodes and the degrees, and checks that the
    /// normalised operators can divide by them.
    fn measure_degrees<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<(), SpectralError> {
        let (graph, transpose, buffers) = (self.graph, self.transpose, &mut self.buffers);
        values_mut(&mut buffers.input)?.fill(1.0);
        let unweighted = Product { weighted: Some(false), ..self.forward };
        buffers.multiply(resources, graph, unweighted, transpose, Source::Input)?;
        for (active, &edges) in self.active.iter_mut().zip(values(&buffers.output)?) {
            *active = edges > 0.0;
        }
        self.dimension = self.active.iter().filter(|&&active| active).count() as u64;
        buffers.multiply(resources, graph, self.forward, transpose, Source::Input)?;
        self.degrees.copy_from_slice(values(&buffers.output)?);
        if self.operator.normalized() {
            for (node, (&active, &degree)) in (0..).zip(self.active.iter().zip(&self.degrees)) {
                if active && (degree.is_nan() || degree <= 0.0) {
                    return Err(SpectralError::NonPositiveDegree { node, degree });
                }
            }
            for ((scale, &active), &degree) in
                self.scale.iter_mut().zip(&self.active).zip(&self.degrees)
            {
                *scale = if active { degree.sqrt().recip() } else { 0.0 };
            }
        }
        Ok(())
    }

    /// Refuses `A` unless `A·r = Aᵀ·r` for a random `r` (Freivalds' check:
    /// an asymmetric `A` passes with probability zero).
    fn check_symmetric<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
    ) -> Result<(), SpectralError> {
        let (graph, buffers) = (self.graph, &mut self.buffers);
        for (node, value) in (0..).zip(values_mut(&mut buffers.input)?) {
            *value = random::uniform(SYMMETRY_SEED, 0, node);
        }
        let Resources { workers, memory, interrupts } = &mut *resources;
        let (forward, transpose) = (self.forward, Product { transpose: true, ..self.forward });
        spmv(workers, memory, *interrupts, graph, &forward, &buffers.input, &mut buffers.output)?;
        spmv(
            workers,
            memory,
            *interrupts,
            graph,
            &transpose,
            &buffers.input,
            &mut buffers.scratch,
        )?;
        let (ax, atx) = (values(&buffers.output)?, values(&buffers.scratch)?);
        let size = dense::max(resources, ax.len(), |i| ax[i].abs().max(atx[i].abs()))?;
        let difference = dense::max(resources, ax.len(), |i| (ax[i] - atx[i]).abs())?;
        if difference.is_nan() || difference > SYMMETRY_TOLERANCE * size {
            return Err(SpectralError::NotSymmetric);
        }
        Ok(())
    }
}

/// Whether every CSR `relations` selects has a reverse CSR equal to its
/// forward one, as an undirected graph's are: then `A = Aᵀ` exactly, and the
/// transposed product would read the same arrays in the same order, so
/// `(A·x + Aᵀ·x)/2` is `A·x` to the bit (where `A·x + A·x` is finite: the two
/// products overflow to infinity above `f64::MAX / 2`, the one product does not). Without reverse CSRs this is not
/// known without a transpose, so it answers false. An unknown relation
/// also answers false (the product reports it). `per_relation`, the CSRs
/// are every selected relation's own, which the product reads: the merged
/// CSR can be symmetric where their sum is not.
fn reverses_are_forward(
    graph: &GraphCsr,
    relations: RelationSelection<'_>,
    per_relation: bool,
) -> bool {
    let selected = match (per_relation, relations) {
        (true, RelationSelection::All) => Ok(graph.relations().iter().collect()),
        _ => graph.selected(relations),
    };
    selected.is_ok_and(|selected| {
        selected.iter().all(|csrs| csrs.reverse().is_some_and(|reverse| reverse == csrs.forward()))
    })
}

/// Which buffer an SpMV multiplies.
#[derive(Debug, Clone, Copy)]
enum Source {
    Input,
    Scaled,
}

/// The `f64` vectors one application of an operator uses.
#[derive(Debug)]
struct Buffers {
    input: AnyVector,
    /// `D^-1/2 · input`, for the normalised operators (else empty).
    scaled: AnyVector,
    output: AnyVector,
    scratch: AnyVector,
}

impl Buffers {
    fn new<I>(
        resources: &mut Resources<'_, I>,
        id_space: u64,
        normalized: bool,
    ) -> Result<Self, ComputeError> {
        let vector = |rows: u64| -> Result<AnyVector, ComputeError> {
            Ok(Vector::filled(resources.memory, rows, 1, 0.0f64)?.into())
        };
        let scaled = vector(if normalized { id_space } else { 0 })?;
        Ok(Self {
            input: vector(id_space)?,
            scaled,
            output: vector(id_space)?,
            scratch: vector(id_space)?,
        })
    }

    /// `output = S · source` for `product`: `(A + Aᵀ)/2` with `transpose`.
    fn multiply<I: Interrupts>(
        &mut self,
        resources: &mut Resources<'_, I>,
        graph: &GraphCsr,
        product: Product<'_>,
        transpose: bool,
        source: Source,
    ) -> Result<(), SpectralError> {
        let x = match source {
            Source::Input => &self.input,
            Source::Scaled => &self.scaled,
        };
        let Resources { workers, memory, interrupts } = &mut *resources;
        spmv(workers, memory, *interrupts, graph, &product, x, &mut self.output)?;
        if transpose {
            let transpose = Product { transpose: true, ..product };
            spmv(workers, memory, *interrupts, graph, &transpose, x, &mut self.scratch)?;
            let scratch = values(&self.scratch)?;
            dense::update(resources, values_mut(&mut self.output)?, |i, y| (y + scratch[i]) / 2.0)?;
        }
        Ok(())
    }
}

/// The values of one of the operator's own vectors: an error, never an
/// empty slice, if the vector is not `f64`.
fn values(vector: &AnyVector) -> Result<&[f64], SpectralError> {
    match vector {
        AnyVector::F64(vector) => Ok(vector.values()),
        other => {
            Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found: other.dtype() }.into())
        }
    }
}

/// The values of one of the operator's own vectors, mutably: an error,
/// never an empty slice, if the vector is not `f64` or a copy fails.
fn values_mut(vector: &mut AnyVector) -> Result<&mut [f64], SpectralError> {
    match vector {
        AnyVector::F64(vector) => Ok(vector.values_mut()?),
        other => {
            Err(ComputeError::DtypeMismatch { expected: Dtype::F64, found: other.dtype() }.into())
        }
    }
}

#[cfg(test)]
#[cfg(not(miri))]
mod tests;
