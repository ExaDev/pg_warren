//! Leiden (DESIGN.md §6.7; Traag, Waltman and van Eck, "From Louvain to
//! Leiden: guaranteeing well-connected communities", 2019): levels of
//! local moving and refinement, each level's graph the quotient of the
//! level before by its refined partition, all held in memory.
//!
//! A level moves nodes between communities ([`local_moving`], from the
//! partition the level below found), refines each community into
//! well-connected parts ([`refine`]) and contracts its own view by the
//! refined partition into the next level's ([`Undirected::contract`]),
//! which starts from the unrefined partition: a refined community's
//! members share one community, so the next level's node takes the least
//! of their communities' labels. A level's nodes are the refined
//! communities of the level below, keyed by the least dense id of their
//! members in the graph, so every label is a dense id of the graph. The
//! run goes on while there is something to aggregate: it stops when a
//! level's moving leaves every node alone, when the refinement does (the
//! next level would be this one), or after `max_levels`; in the last two
//! cases it returns the refined partition, so every exit returns a
//! connected one. That multilevel pass is one iteration; each later one
//! starts its first level's moving from the partition the one before
//! found. The run stops after `iterations`, or after an iteration whose
//! partition is the one it started from: with the same start and seed,
//! the next would find it again.
//!
//! Only the graph's view and one level's view exist at a time (two while
//! one is contracted into the next). The graph's view is dropped once the
//! second level exists and asked for again ([`Multilevel::view`]) when a
//! later iteration or the modularity needs it.

use super::{
    local_moving, modularity, refine, CommunityError, LocalMoving, Moved, Refined, Refinement,
    Undirected,
};
use crate::compute::{ComputeMemory, MemoryCharge, Vector, Workers};
use crate::graph::{DenseId, Membership, Tombstones};
use crate::interrupts::Interrupts;

/// The most iterations a run until stable makes.
pub const MOST_ITERATIONS: u32 = 100;

/// The sweeps a level's local moving makes at most.
const MAX_SWEEPS: u32 = 100;

/// How a Leiden run goes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Leiden {
    /// The resolution `γ`.
    pub resolution: f64,
    /// Orders the nodes and seeds the refinement's draws, every level and
    /// every iteration alike.
    pub seed: u64,
    /// The most levels of an iteration, at least 1.
    pub max_levels: u32,
    /// A level's sweep that raises modularity by no more than this is its
    /// last.
    pub tolerance: f64,
    /// The randomness `θ` of the refinement.
    pub randomness: f64,
    /// The most iterations, at least 1; `None`: until an iteration changes
    /// nothing, at most [`MOST_ITERATIONS`].
    pub iterations: Option<u32>,
}

/// The partition a run found.
#[derive(Debug)]
pub struct Found {
    /// Each live node's community, labelled by the least dense id of its
    /// members (tombstoned nodes hold 0).
    pub labels: Vector<i64>,
    /// Its modularity on the graph's view.
    pub modularity: f64,
    /// False when a run until stable stopped at [`MOST_ITERATIONS`] with
    /// the partition still changing.
    pub stable: bool,
}

/// What a run reads and reports outside the domain.
pub trait Multilevel {
    /// Why the port failed; a run's own errors convert into it.
    type Error: From<CommunityError>;
    /// The graph's undirected view, built afresh at each call.
    fn view(&mut self) -> Result<Undirected, Self::Error>;
    /// Level `level` (from 1) of iteration `iteration` (from 1) starts.
    fn level(&mut self, iteration: u32, level: u32) -> Result<(), Self::Error>;
    /// Whether it keeps each level's partition ([`Self::keep_level`]).
    fn keeps_levels(&self) -> bool;
    /// The partition of the graph that the run's `ordinal`th level (from
    /// 1, numbered on across iterations) found, labelled as
    /// [`Found::labels`].
    fn keep_level(&mut self, ordinal: u32, labels: Vector<i64>) -> Result<(), Self::Error>;
}

/// Runs Leiden on the graph `run` reads, whose deleted nodes `tombstones`
/// holds, as `options` says.
pub fn leiden<I: Interrupts, M: Multilevel>(
    workers: &Workers,
    memory: &ComputeMemory,
    interrupts: &mut I,
    tombstones: &Tombstones,
    options: Leiden,
    run: &mut M,
) -> Result<Found, M::Error> {
    let mut runner = Runner { workers, memory, interrupts, tombstones, options, run, view: None };
    let mut ordinal = 0;
    let mut previous: Option<Vector<i64>> = None;
    let mut iteration = 0;
    let (labels, stable) = loop {
        iteration += 1;
        let start = previous.as_ref().map(Vector::values);
        let found = runner.iteration(iteration, start, &mut ordinal)?;
        if options.iterations == Some(iteration) {
            break (found, true);
        }
        // Labels are the least dense id of each community's members, so
        // the partitions are the same exactly when the labels are.
        if previous.as_ref().is_some_and(|previous| previous.values() == found.values()) {
            break (found, true);
        }
        if options.iterations.is_none() && iteration == MOST_ITERATIONS {
            break (found, false);
        }
        previous = Some(found);
    };
    drop(previous);
    let view = runner.graph_view()?;
    let modularity = modularity(memory, &view, labels.values(), options.resolution)?;
    Ok(Found { labels, modularity, stable })
}

/// A run's inputs, and the graph's view while it is held.
struct Runner<'a, I, M> {
    workers: &'a Workers,
    memory: &'a ComputeMemory,
    interrupts: &'a mut I,
    tombstones: &'a Tombstones,
    options: Leiden,
    run: &'a mut M,
    view: Option<Undirected>,
}

/// A level's nodes beyond the first's: each one's key (the least dense id
/// of its members in the graph), the community it starts in, and each
/// graph node's node of the level.
struct Above {
    keys: Vec<i64>,
    initial: Vec<i64>,
    members: Vec<u64>,
    tombstones: Tombstones,
    _charge: MemoryCharge,
}

impl<I: Interrupts, M: Multilevel> Runner<'_, I, M> {
    /// The graph's view: the one held, or a new one.
    fn graph_view(&mut self) -> Result<Undirected, M::Error> {
        match self.view.take() {
            Some(view) => Ok(view),
            None => self.run.view(),
        }
    }

    /// One iteration's levels, from the partition `start` (each node alone
    /// if `None`); returns the partition it found.
    fn iteration(
        &mut self,
        iteration: u32,
        start: Option<&[i64]>,
        ordinal: &mut u32,
    ) -> Result<Vector<i64>, M::Error> {
        let mut view = self.graph_view()?;
        let mut above: Option<Above> = None;
        let mut level = 0;
        loop {
            level += 1;
            self.run.level(iteration, level)?;
            self.interrupts.check().map_err(CommunityError::from)?;
            let tombstones = above.as_ref().map_or(self.tombstones, |above| &above.tombstones);
            let nodes = view.id_space() - tombstones.count();
            let keys = above.as_ref().map(|above| &above.keys[..]);
            let initial = above.as_ref().map_or(start, |above| Some(&above.initial[..]));
            let options = self.options;
            let moving = LocalMoving {
                resolution: options.resolution,
                seed: options.seed,
                max_sweeps: MAX_SWEEPS,
                tolerance: options.tolerance,
            };
            let moved = local_moving(
                self.workers,
                self.memory,
                self.interrupts,
                &view,
                tombstones,
                initial,
                keys,
                moving,
            )?;
            // Every node alone: the partition is this level's nodes, the
            // refined partition of the level below, which is connected.
            let refined = match moved.communities < nodes {
                true => Some(self.refined(&view, tombstones, keys, &moved)?),
                false => None,
            };
            let members = above.as_ref().map(|above| &above.members[..]);
            let (keeps_refined, last) = match &refined {
                None => (false, true),
                // Stopping here, return the refined partition, which is
                // connected, as Traag et al.'s is; the moved one need not be.
                Some(refined) => {
                    let last = level >= options.max_levels || refined.communities == nodes;
                    (last, last)
                }
            };
            let kept = match (&refined, keeps_refined) {
                (Some(refined), true) => refined.labels.values(),
                _ => moved.labels.values(),
            };
            if self.run.keeps_levels() {
                *ordinal += 1;
                let lifted = self.lift(kept, members)?;
                self.run.keep_level(*ordinal, lifted)?;
            }
            let refined = match refined {
                Some(refined) if !last => refined,
                _ => {
                    let found = self.lift(kept, members)?;
                    if level == 1 {
                        self.view = Some(view);
                    }
                    return Ok(found);
                }
            };
            let next = self.aggregate(&view, tombstones, &moved, &refined, above.as_ref())?;
            drop((moved, refined));
            view = next.0;
            above = Some(next.1);
        }
    }

    /// The refinement of `moved` on `view`.
    fn refined(
        &mut self,
        view: &Undirected,
        tombstones: &Tombstones,
        keys: Option<&[i64]>,
        moved: &Moved,
    ) -> Result<Refined, CommunityError> {
        let refinement = Refinement {
            resolution: self.options.resolution,
            seed: self.options.seed,
            randomness: self.options.randomness,
        };
        refine(
            self.workers,
            self.memory,
            self.interrupts,
            view,
            tombstones,
            moved.labels.values(),
            keys,
            refinement,
        )
    }

    /// The partition `labels` of a level's nodes as a partition of the
    /// graph: each live node takes the label of its node of the level
    /// (`members`; itself on the first level), tombstoned nodes 0.
    fn lift(&self, labels: &[i64], members: Option<&[u64]>) -> Result<Vector<i64>, CommunityError> {
        let mut lifted = Vector::filled(self.memory, self.tombstones.id_space(), 1, 0i64)?;
        let values = lifted.values_mut()?;
        for (node, value) in values.iter_mut().enumerate() {
            if !self.tombstones.contains(node as DenseId) {
                let at = members.map_or(node, |members| members[node] as usize);
                *value = labels[at];
            }
        }
        Ok(lifted)
    }

    /// The next level: `view` contracted by the refined partition, whose
    /// communities are its nodes in the order of their labels, each
    /// starting in the least label of its members' moved communities.
    fn aggregate(
        &mut self,
        view: &Undirected,
        tombstones: &Tombstones,
        moved: &Moved,
        refined: &Refined,
        below: Option<&Above>,
    ) -> Result<(Undirected, Above), CommunityError> {
        let nodes = view.id_space() as usize;
        let refined = refined.labels.values();
        let live = |node: usize| !tombstones.contains(node as DenseId);
        let count = refined.len() as u64;
        let graph_nodes = self.tombstones.id_space() as usize;
        let charge = self.memory.charge(
            (count.saturating_mul(3).saturating_add(graph_nodes as u64)).saturating_mul(8),
        )?;
        let mut keys = reserved(nodes)?;
        keys.extend((0..nodes).filter(|&node| live(node)).map(|node| refined[node]));
        keys.sort_unstable();
        keys.dedup();
        let communities = keys.len();
        let mut stored = reserved(nodes)?;
        stored.extend((0..nodes).map(|node| match live(node) {
            true => keys.binary_search(&refined[node]).map_or(u64::MAX, |at| at as u64),
            false => u64::MAX,
        }));
        let membership = Membership::from_stored(stored, communities as u64)?;
        let next = view.contract(self.memory, self.interrupts, &membership)?;
        let mut initial = reserved(communities)?;
        initial.resize(communities, i64::MAX);
        let mut members = reserved(graph_nodes)?;
        let to_next = |node: u64| membership.community(node).map_or(u64::MAX, |next| next);
        for (node, &community) in moved.labels.values().iter().enumerate() {
            if let Some(next) = membership.community(node as DenseId) {
                let least = &mut initial[next as usize];
                *least = (*least).min(community);
            }
        }
        match below {
            None => members.extend((0..graph_nodes as u64).map(to_next)),
            Some(below) => members.extend(below.members.iter().map(|&node| match node {
                u64::MAX => u64::MAX,
                node => to_next(node),
            })),
        }
        let tombstones = Tombstones::new(communities as u64)?;
        Ok((next, Above { keys, initial, members, tombstones, _charge: charge }))
    }
}

/// An empty buffer with room for `len` elements, reserved fallibly; the
/// caller charged them.
fn reserved<T>(len: usize) -> Result<Vec<T>, CommunityError> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|_| crate::compute::ComputeError::OutOfMemory {
        bytes: (len * std::mem::size_of::<T>()) as u64,
    })?;
    Ok(buffer)
}

// Not under Miri: the compute pool's work stealing runs through
// crossbeam-epoch, which Miri's Stacked Borrows model rejects.
#[cfg(test)]
#[cfg(not(miri))]
mod tests;
