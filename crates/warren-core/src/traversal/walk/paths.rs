//! The walks themselves, node by node with the relation of each hop
//! (`mode = 'paths'`).
//!
//! With distinct ends, the fast-path search records how it first reached
//! each node at each level, and an end's path is read back from those
//! records as soon as the end is found. Without, every walk is a path:
//! the walk counts are kept for every level, pruned backwards to the nodes
//! that can still finish a walk, and enumerated depth-first, so every
//! branch the enumeration takes yields at least one path. The number of
//! walks can grow exponentially with the hops, so callers bound it with
//! `max_results` (DESIGN.md §14 item 15).

use std::ops::ControlFlow;

use super::super::scratch::{NodeMap, Visited};
use super::super::stats::Progress;
use super::super::{TraversalError, TraversalStats};
use super::plan::{plan, WalkPlan};
use super::reach::{reach, Labelled, Level, Observer};
use super::tally::{tally, Tally};
use super::{RowLimit, Step, WalkOptions};
use crate::graph::{memory, DenseId, GraphCsr, GraphError, RelationIndex};
use crate::interrupts::Interrupts;

/// One walk, from its start to its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkPath {
    /// The nodes, start first. In reverse, the start is the reported node
    /// and the end one of the allowed ends.
    pub nodes: Vec<DenseId>,
    /// The relation of each hop: `relations[i]` links `nodes[i]` to
    /// `nodes[i + 1]`.
    pub relations: Vec<RelationIndex>,
}

impl WalkPath {
    /// Appends a hop along `relation` to `node`. Paths are short, so they
    /// grow by doubling rather than by [`memory::reserve_one`]'s
    /// graph-sized steps.
    fn push(&mut self, relation: RelationIndex, node: DenseId) -> Result<(), TraversalError> {
        let out_of_memory = |elements| GraphError::OutOfMemory { elements };
        self.nodes.try_reserve(1).map_err(|_| out_of_memory(self.nodes.len() + 1))?;
        self.relations.try_reserve(1).map_err(|_| out_of_memory(self.relations.len() + 1))?;
        self.nodes.push(node);
        self.relations.push(relation);
        Ok(())
    }

    /// A copy, or an error if there is no memory for it.
    fn try_clone(&self) -> Result<Self, TraversalError> {
        let mut nodes = memory::with_capacity(self.nodes.len())?;
        nodes.extend_from_slice(&self.nodes);
        let mut relations = memory::with_capacity(self.relations.len())?;
        relations.extend_from_slice(&self.relations);
        Ok(Self { nodes, relations })
    }
}

/// The walks along `steps` from `starts` (or, in reverse, ending in
/// `starts`), each handed to `visitor` as it is found: one per distinct end
/// if `options.distinct_ends`, else every walk. Returning
/// [`ControlFlow::Break`] stops the walk after that path, which is not
/// truncation; stopping because another path would exceed
/// `options.max_results` is.
pub fn walk_paths_each(
    graph: &GraphCsr,
    starts: &[DenseId],
    steps: &[Step],
    options: WalkOptions,
    interrupts: &mut dyn Interrupts,
    mut visitor: impl FnMut(WalkPath) -> ControlFlow<()>,
) -> Result<TraversalStats, TraversalError> {
    let plan = plan(graph, starts, steps, options.reverse)?;
    let mut progress = Progress::new(interrupts);
    let mut limit = RowLimit::new(options.max_results);
    let mut report = |mut path: WalkPath| {
        if plan.reverse {
            path.nodes.reverse();
            path.relations.reverse();
        }
        limit.offer(|| visitor(path))
    };
    match options.distinct_ends {
        true => {
            let mut recorder =
                Recorder { id_space: plan.id_space, levels: Vec::new(), report: &mut report };
            reach::<Labelled>(&plan, &mut progress, &mut recorder)?;
        }
        false => enumerate(&plan, &mut progress, &mut report)?,
    }
    limit.record(&mut progress);
    Ok(progress.finish())
}

/// How a node was first reached at a level.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Came {
    /// Not reached: the empty slot of a dense record.
    Absent,
    /// By a hop from `via` along `relation`.
    Hop { via: DenseId, relation: RelationIndex },
    /// Handed on by the previous step after `depth` hops.
    HandedOn { depth: u32 },
}

/// Records how the search reached each node at each level, and reports an
/// end's path as soon as it is found.
struct Recorder<R> {
    id_space: u64,
    /// `levels[step][depth]`.
    levels: Vec<Vec<NodeMap<Came>>>,
    report: R,
}

impl<R: FnMut(WalkPath) -> ControlFlow<()>> Recorder<R> {
    fn record(&mut self, level: Level, node: DenseId, came: Came) -> Result<(), TraversalError> {
        while self.levels.len() <= level.step {
            memory::reserve_one(&mut self.levels)?;
            self.levels.push(Vec::new());
        }
        let depths = &mut self.levels[level.step];
        while depths.len() <= level.depth as usize {
            memory::reserve_one(depths)?;
            depths.push(NodeMap::new(self.id_space, Came::Absent));
        }
        depths[level.depth as usize].insert(node, came)
    }

    fn came(&self, level: Level, node: DenseId) -> Option<Came> {
        self.levels.get(level.step)?.get(level.depth as usize)?.get(node)
    }

    /// The walk the records lead back from `node` at `level`, in execution
    /// order.
    fn trail(&self, level: Option<Level>, node: DenseId) -> Result<WalkPath, TraversalError> {
        let mut path = WalkPath { nodes: vec![node], relations: Vec::new() };
        let (mut at, mut node) = (level, node);
        while let Some(level) = at {
            at = match self.came(level, node) {
                Some(Came::Hop { via, relation }) => {
                    path.push(relation, via)?;
                    node = via;
                    Some(Level { step: level.step, depth: level.depth.saturating_sub(1) })
                }
                Some(Came::HandedOn { depth }) => {
                    level.step.checked_sub(1).map(|step| Level { step, depth })
                }
                Some(Came::Absent) | None => None,
            };
        }
        path.nodes.reverse();
        path.relations.reverse();
        Ok(path)
    }
}

impl<R: FnMut(WalkPath) -> ControlFlow<()>> Observer<RelationIndex> for Recorder<R> {
    fn reached(
        &mut self,
        level: Level,
        node: DenseId,
        via: DenseId,
        relation: RelationIndex,
    ) -> Result<(), TraversalError> {
        self.record(level, node, Came::Hop { via, relation })
    }

    fn handed_on(&mut self, level: Level, node: DenseId) -> Result<(), TraversalError> {
        let next = Level { step: level.step + 1, depth: 0 };
        self.record(next, node, Came::HandedOn { depth: level.depth })
    }

    fn ended(
        &mut self,
        level: Option<Level>,
        node: DenseId,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let path = self.trail(level, node)?;
        Ok((self.report)(path))
    }
}

/// Reports every walk of `plan`, depth-first.
fn enumerate(
    plan: &WalkPlan<'_>,
    progress: &mut Progress<'_>,
    report: &mut impl FnMut(WalkPath) -> ControlFlow<()>,
) -> Result<(), TraversalError> {
    let mut levels: Vec<Vec<Tally>> = Vec::new();
    tally(plan, progress, |level, tally| {
        if levels.len() <= level.step {
            memory::reserve_one(&mut levels)?;
            levels.push(Vec::new());
        }
        let depths = &mut levels[level.step];
        memory::reserve_one(depths)?;
        depths.push(tally);
        Ok(())
    })?;
    let live = Live::prune(plan, &levels, progress)?;
    let mut path = WalkPath { nodes: Vec::new(), relations: Vec::new() };
    for &start in &plan.starts {
        path.nodes.clear();
        path.relations.clear();
        memory::reserve_one(&mut path.nodes)?;
        path.nodes.push(start);
        let flow = match plan.steps.is_empty() {
            true => report(path.try_clone()?),
            false if live.contains(Level { step: 0, depth: 0 }, start) => {
                live.walk_from(start, &mut path, progress, report)?
            }
            false => ControlFlow::Continue(()),
        };
        if flow.is_break() {
            break;
        }
    }
    Ok(())
}

/// The nodes at each level that can still finish a walk.
struct Live<'p, 'w> {
    plan: &'p WalkPlan<'w>,
    /// `levels[step][depth]`; a level past the end of a step's levels has
    /// no live nodes.
    levels: Vec<Vec<Visited>>,
}

/// Where a walk can go next from a node at a level.
#[derive(Debug, Clone, Copy)]
enum Choice {
    /// End the walk here.
    End,
    /// Leave the step for the next one.
    HandOn,
    /// Take a hop.
    Hop(RelationIndex, DenseId),
}

/// A node on the enumeration's stack with the choices it has left.
struct Frame {
    level: Level,
    node: DenseId,
    /// Whether the node was entered by a hop (rather than a hand-on, or as
    /// a start), so leaving it pops the path.
    hopped: bool,
    choices: Vec<Choice>,
    next: usize,
}

impl<'p, 'w> Live<'p, 'w> {
    /// Prunes `reached` (every level's nodes, from the tally) backwards to
    /// the nodes that can finish a walk.
    fn prune(
        plan: &'p WalkPlan<'w>,
        reached: &[Vec<Tally>],
        progress: &mut Progress<'_>,
    ) -> Result<Self, TraversalError> {
        let mut live = Self { plan, levels: Vec::new() };
        for depths in reached {
            memory::reserve_one(&mut live.levels)?;
            let mut levels = memory::with_capacity(depths.len())?;
            levels.extend(depths.iter().map(|_| Visited::new(plan.id_space)));
            live.levels.push(levels);
        }
        for (step, depths) in reached.iter().enumerate().rev() {
            for (depth, tally) in depths.iter().enumerate().rev() {
                let level = Level { step, depth: depth as u32 };
                for (node, _) in tally.iter() {
                    if live.has_choice(level, node, progress)? {
                        live.levels[step][depth].insert(node)?;
                    }
                }
            }
        }
        Ok(live)
    }

    fn contains(&self, level: Level, node: DenseId) -> bool {
        self.levels
            .get(level.step)
            .and_then(|depths| depths.get(level.depth as usize))
            .is_some_and(|live| live.contains(node))
    }

    /// Whether `node` at `level` can finish a walk, given the levels after
    /// it.
    fn has_choice(
        &self,
        level: Level,
        node: DenseId,
        progress: &mut Progress<'_>,
    ) -> Result<bool, TraversalError> {
        let mut found = false;
        self.choices(level, node, progress, |_| {
            found = true;
            Ok(ControlFlow::Break(()))
        })?;
        Ok(found)
    }

    /// Hands each choice `node` has at `level` to `choice`, until it breaks.
    fn choices(
        &self,
        level: Level,
        node: DenseId,
        progress: &mut Progress<'_>,
        mut choice: impl FnMut(Choice) -> Result<ControlFlow<()>, TraversalError>,
    ) -> Result<(), TraversalError> {
        let step = &self.plan.steps[level.step];
        if level.depth >= step.hops.min() && step.exits(node) {
            let last = level.step + 1 == self.plan.steps.len();
            let next = Level { step: level.step + 1, depth: 0 };
            if last && choice(Choice::End)?.is_break() {
                return Ok(());
            }
            if !last && self.contains(next, node) && choice(Choice::HandOn)?.is_break() {
                return Ok(());
            }
        }
        if level.depth < step.hops.max() && step.leaves(node) {
            let deeper = Level { step: level.step, depth: level.depth + 1 };
            for (relation, neighbour) in step.adjacency.labelled_neighbours(node) {
                progress.edge_traversed()?;
                if self.contains(deeper, neighbour)
                    && choice(Choice::Hop(relation, neighbour))?.is_break()
                {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// Reports every walk from `start`, whose path holds just `start`.
    fn walk_from(
        &self,
        start: DenseId,
        path: &mut WalkPath,
        progress: &mut Progress<'_>,
        report: &mut impl FnMut(WalkPath) -> ControlFlow<()>,
    ) -> Result<ControlFlow<()>, TraversalError> {
        let mut stack = Vec::new();
        self.push(&mut stack, Level { step: 0, depth: 0 }, start, false, progress)?;
        while let Some(frame) = stack.last_mut() {
            let Some(&choice) = frame.choices.get(frame.next) else {
                if frame.hopped {
                    path.nodes.pop();
                    path.relations.pop();
                }
                stack.pop();
                continue;
            };
            frame.next += 1;
            let (level, node) = (frame.level, frame.node);
            match choice {
                Choice::End => {
                    if report(path.try_clone()?).is_break() {
                        return Ok(ControlFlow::Break(()));
                    }
                }
                Choice::HandOn => {
                    let next = Level { step: level.step + 1, depth: 0 };
                    self.push(&mut stack, next, node, false, progress)?;
                }
                Choice::Hop(relation, neighbour) => {
                    path.push(relation, neighbour)?;
                    let deeper = Level { step: level.step, depth: level.depth + 1 };
                    self.push(&mut stack, deeper, neighbour, true, progress)?;
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    fn push(
        &self,
        stack: &mut Vec<Frame>,
        level: Level,
        node: DenseId,
        hopped: bool,
        progress: &mut Progress<'_>,
    ) -> Result<(), TraversalError> {
        let mut choices = Vec::new();
        self.choices(level, node, progress, |choice| {
            memory::reserve_one(&mut choices)?;
            choices.push(choice);
            Ok(ControlFlow::Continue(()))
        })?;
        memory::reserve_one(stack)?;
        stack.push(Frame { level, node, hopped, choices, next: 0 });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ops::ControlFlow;

    use super::super::example::{knowledge_graph, step, AFFILIATED_WITH, AUTHORED_BY, CITES};
    use super::super::{
        walk, walk_paths_each, HopRange, Step, WalkMode, WalkOptions, WalkPath, WalkResult,
    };
    use crate::graph::Direction;
    use crate::graph::{DenseId, GraphCsr};
    use crate::interrupts::Uninterrupted;
    use crate::traversal::example::{directed, graph, relations};
    use crate::traversal::Traversed;

    fn paths_of(
        graph: &GraphCsr,
        starts: &[DenseId],
        steps: &[Step],
        options: WalkOptions,
    ) -> Traversed<Vec<(Vec<DenseId>, Vec<usize>)>> {
        let found =
            walk(graph, starts, steps, WalkMode::Paths, options, &mut Uninterrupted).unwrap();
        let WalkResult::Paths(paths) = found.result else { panic!("not paths: {found:?}") };
        let mut paths: Vec<_> = paths
            .into_iter()
            .map(|path| (path.nodes, path.relations.iter().map(|r| r.index()).collect()))
            .collect();
        paths.sort();
        Traversed { result: paths, stats: found.stats }
    }

    fn paths(
        graph: &GraphCsr,
        starts: &[DenseId],
        steps: &[Step],
        options: WalkOptions,
    ) -> Vec<(Vec<DenseId>, Vec<usize>)> {
        paths_of(graph, starts, steps, options).result
    }

    fn walks() -> WalkOptions {
        WalkOptions { distinct_ends: false, ..WalkOptions::default() }
    }

    fn hops(min: u32, max: u32) -> Step {
        Step {
            relations: None,
            direction: Direction::Out,
            filter: None,
            hops: HopRange::new(min, max).unwrap(),
        }
    }

    fn path(nodes: &[DenseId], relations: &[usize]) -> (Vec<DenseId>, Vec<usize>) {
        (nodes.to_vec(), relations.to_vec())
    }

    #[test]
    fn distinct_ends_give_one_path_per_end_with_each_hop_s_relation() {
        let graph = knowledge_graph();
        let steps = [
            step(Some(CITES), 1, 3, None),
            step(Some(AUTHORED_BY), 1, 1, None),
            step(Some(AFFILIATED_WITH), 1, 1, None),
        ];
        let (c, a, f) = (CITES, AUTHORED_BY, AFFILIATED_WITH);
        assert_eq!(
            paths(&graph, &[0], &steps, WalkOptions::default()),
            [path(&[0, 1, 2, 6, 9], &[c, c, a, f]), path(&[0, 1, 5, 8], &[c, a, f])]
        );
    }

    #[test]
    fn without_distinct_ends_every_walk_is_a_path() {
        let graph = knowledge_graph();
        let steps = [
            step(Some(CITES), 1, 3, None),
            step(Some(AUTHORED_BY), 1, 1, None),
            step(Some(AFFILIATED_WITH), 1, 1, Some(&[8])),
        ];
        let (c, a, f) = (CITES, AUTHORED_BY, AFFILIATED_WITH);
        assert_eq!(
            paths(&graph, &[0], &steps, walks()),
            [
                path(&[0, 1, 2, 3, 7, 8], &[c, c, c, a, f]),
                path(&[0, 1, 5, 8], &[c, a, f]),
                path(&[0, 4, 5, 8], &[c, a, f]),
            ]
        );
    }

    #[test]
    fn an_edge_in_two_relations_makes_two_paths() {
        let parallel = graph(3, 2, &[(0, 0, 1, 1.0), (1, 0, 1, 1.0), (0, 1, 2, 1.0)], true);
        assert_eq!(
            paths(&parallel, &[0], &[hops(2, 2)], walks()),
            [path(&[0, 1, 2], &[0, 0]), path(&[0, 1, 2], &[1, 0])]
        );
        assert_eq!(paths(&parallel, &[0], &[hops(2, 2)], WalkOptions::default()).len(), 1);
    }

    #[test]
    fn paths_may_revisit_nodes() {
        let cycle = directed(2, &[(0, 1), (1, 0)]);
        assert_eq!(
            paths(&cycle, &[0], &[hops(2, 3)], walks()),
            [path(&[0, 1, 0], &[0, 0]), path(&[0, 1, 0, 1], &[0, 0, 0])]
        );
        assert_eq!(paths(&cycle, &[0], &[hops(2, 3)], WalkOptions::default()).len(), 2);
    }

    #[test]
    fn the_same_hops_split_differently_between_steps_are_separate_paths() {
        let chain = directed(4, &[(0, 1), (1, 2), (2, 3)]);
        let found = paths(&chain, &[0], &[hops(1, 2), hops(1, 2)], walks());
        assert_eq!(
            found,
            [
                path(&[0, 1, 2], &[0, 0]),
                path(&[0, 1, 2, 3], &[0, 0, 0]),
                path(&[0, 1, 2, 3], &[0, 0, 0]),
            ]
        );
    }

    #[test]
    fn reversed_paths_read_from_the_would_be_start_to_the_allowed_end() {
        let diamond = directed(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        let reverse = WalkOptions { reverse: true, ..walks() };
        assert_eq!(
            paths(&diamond, &[3], &[hops(2, 2)], reverse),
            [path(&[0, 1, 3], &[0, 0]), path(&[0, 2, 3], &[0, 0])]
        );
        let reverse = WalkOptions { reverse: true, ..WalkOptions::default() };
        assert_eq!(paths(&diamond, &[3], &[hops(1, 2)], reverse).len(), 3);
    }

    #[test]
    fn a_walk_of_no_steps_is_a_path_of_its_start() {
        let graph = knowledge_graph();
        assert_eq!(paths(&graph, &[2, 2], &[], walks()), [path(&[2], &[])]);
        assert_eq!(paths(&graph, &[2], &[], WalkOptions::default()), [path(&[2], &[])]);
    }

    #[test]
    fn max_results_truncates_both_kinds_of_enumeration() {
        let diamond = directed(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        for distinct_ends in [false, true] {
            let options = WalkOptions { distinct_ends, max_results: Some(2), reverse: false };
            let found = paths_of(&diamond, &[0], &[hops(1, 2)], options);
            assert_eq!(found.result.len(), 2);
            assert!(found.stats.truncated);
        }
    }

    #[test]
    fn the_streaming_form_stops_when_the_visitor_breaks() {
        let diamond = directed(4, &[(0, 1), (0, 2), (1, 3), (2, 3)]);
        for distinct_ends in [false, true] {
            let options = WalkOptions { distinct_ends, ..WalkOptions::default() };
            let mut seen: Vec<WalkPath> = Vec::new();
            let stats =
                walk_paths_each(&diamond, &[0], &[hops(1, 2)], options, &mut Uninterrupted, |p| {
                    seen.push(p);
                    ControlFlow::Break(())
                })
                .unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].relations, vec![relations(1)[0]; seen[0].nodes.len() - 1]);
            assert!(!stats.truncated);
        }
    }
}
