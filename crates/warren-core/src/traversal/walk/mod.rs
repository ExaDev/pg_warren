//! Walks: typed path patterns executed over CSR without leaving the engine
//! (DESIGN.md §9.2, `warren.walk`).
//!
//! A walk is a chain of [`Step`]s. Each step follows its relations in its
//! direction for a number of hops in its [`HopRange`], entering only nodes
//! in its filter; the nodes it ends on start the next step. Hops are walks,
//! not paths: a walk may repeat nodes and edges, as a recursive CTE with a
//! depth column does.

mod ends;
mod plan;
mod reach;
mod step;

use std::ops::ControlFlow;

pub use ends::walk_ends_each;
pub use step::{HopRange, Step};

use super::scratch::Rows;
use super::stats::Progress;
use super::{Interrupts, TraversalError, Traversed};
use crate::graph::{DenseId, GraphCsr};

/// What a walk returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkMode {
    /// The end frontier: each node a walk ends on.
    Ends,
}

/// How a walk reports its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WalkOptions {
    /// Report each end once, however many walks end on it.
    pub distinct_ends: bool,
    /// The most rows to report; stopping because of it is truncation.
    pub max_results: Option<u64>,
    /// Execute the chain from its last step backwards, taking the starts as
    /// the allowed ends and reporting the nodes walks would start from.
    pub reverse: bool,
}

impl Default for WalkOptions {
    /// Distinct ends, no row limit, forwards: the `warren.walk` defaults.
    fn default() -> Self {
        Self { distinct_ends: true, max_results: None, reverse: false }
    }
}

/// A walk's result, by [`WalkMode`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WalkResult {
    /// [`WalkMode::Ends`].
    Ends(Vec<DenseId>),
}

/// Walks `steps` from `starts` over `graph` and collects the result `mode`
/// asks for. The streaming forms are [`walk_ends_each`].
pub fn walk(
    graph: &GraphCsr,
    starts: &[DenseId],
    steps: &[Step],
    mode: WalkMode,
    options: WalkOptions,
    interrupts: &mut dyn Interrupts,
) -> Result<Traversed<WalkResult>, TraversalError> {
    match mode {
        WalkMode::Ends => {
            let mut ends = Rows::new();
            let stats =
                walk_ends_each(graph, starts, steps, options, interrupts, |end| ends.push(end))?;
            Ok(Traversed { result: WalkResult::Ends(ends.finish()?), stats })
        }
    }
}

/// Counts the rows a walk reports against `max_results`.
struct RowLimit {
    max: Option<u64>,
    reported: u64,
    truncated: bool,
}

impl RowLimit {
    fn new(max: Option<u64>) -> Self {
        Self { max, reported: 0, truncated: false }
    }

    /// Reports a row with `report`, or stops the walk if the limit is
    /// reached.
    fn offer(&mut self, report: impl FnOnce() -> ControlFlow<()>) -> ControlFlow<()> {
        if self.max == Some(self.reported) {
            self.truncated = true;
            return ControlFlow::Break(());
        }
        self.reported += 1;
        report()
    }

    /// Marks `progress` truncated if the limit stopped the walk.
    fn record(&self, progress: &mut Progress<'_>) {
        if self.truncated {
            progress.truncate();
        }
    }
}
