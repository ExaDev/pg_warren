//! `warren.leiden` (DESIGN.md §6.7): Leiden community detection, run by
//! `warren_core::community::leiden` with its levels in memory, each
//! contracted from the one before. The port over a version builds the
//! graph's undirected view, reports each level as the command of the
//! statement (as `warren.report_progress` does for the PL/pgSQL library),
//! and persists each level's partition when `levels_prefix` asks; `out` is
//! the only other thing written.

use pgrx::prelude::*;
use warren_core::community::{leiden as run, Leiden, Multilevel, Undirected, MOST_ITERATIONS};
use warren_core::compute::Vector;

use super::super::modularity::require_resolution;
use super::super::partition::undirected;
use super::super::CommunitySqlError;
use crate::access::entry;
use crate::activity::{report_command, Report, Running};
use crate::cache::{resolve, CachedIds};
use crate::compute_sql::Vectors;
use crate::interrupts::{workers, PostgresInterrupts};
use crate::source::with_id_type;

/// The prefix of the names the library keeps for its own intermediates.
const RESERVED_PREFIX: &str = "_warren_";

/// `warren.leiden`: Leiden community detection (Traag, Waltman and van
/// Eck 2019) on `graph`'s version `version` (NULL: the current one),
/// writing vector `out` (`i64`, each live node's community labelled by
/// the least dense id of its members) and returning its modularity at
/// `resolution`. Each level moves nodes between communities from the
/// partition the level below found, refines each community into
/// well-connected parts (θ = `randomness`) and contracts its graph by the
/// refined partition into the next level's; it stops when a level's
/// moving leaves every node alone, when the refinement does, or after
/// `max_levels`, and a level's moving stops when a sweep raises
/// modularity by no more than `tolerance`. That multilevel pass is one
/// iteration; each later one starts from the partition the one before
/// found, until `iterations` have run or one changes nothing (a negative
/// `iterations`: until then, at most 100, with a WARNING if still
/// changing). The graph is undirected for this (`weighted` and
/// `per_relation` as `warren.modularity`'s); `seed` orders the nodes and
/// seeds the draws. `levels_prefix` keeps every level's partition, in
/// turn across iterations, as `<prefix>_1`, `<prefix>_2`, …, so the last
/// is `out`.
#[allow(clippy::too_many_arguments)] // the SQL signature of DESIGN.md §6.7
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.leiden(graph text, "out" text, resolution double precision DEFAULT 1.0,
                              seed bigint DEFAULT 0, max_levels integer DEFAULT 100,
                              tolerance double precision DEFAULT 1e-7,
                              randomness double precision DEFAULT 0.01, iterations integer DEFAULT 2,
                              weighted boolean DEFAULT NULL, levels_prefix text DEFAULT NULL,
                              version bigint DEFAULT NULL, per_relation boolean DEFAULT false)
RETURNS double precision
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'leiden_wrapper';
"#
)]
fn leiden(
    graph: Option<&str>,
    out: Option<&str>,
    resolution: default!(Option<f64>, "1.0"),
    seed: default!(Option<i64>, "0"),
    max_levels: default!(Option<i32>, "100"),
    tolerance: default!(Option<f64>, "1e-7"),
    randomness: default!(Option<f64>, "0.01"),
    iterations: default!(Option<i32>, "2"),
    weighted: default!(Option<bool>, "NULL"),
    levels_prefix: default!(Option<&str>, "NULL"),
    version: default!(Option<i64>, "NULL"),
    per_relation: default!(Option<bool>, "false"),
) -> Option<f64> {
    entry(|_| -> Result<Option<f64>, CommunitySqlError> {
        // The graph's current version is pinned, and read access checked,
        // before the arguments are, as the PL/pgSQL library did.
        let pinned = match (graph, version) {
            (Some(graph), None) => Some(resolve(graph, None, "read")?),
            _ => None,
        };
        let out = out.ok_or(CommunitySqlError::NullArgument("out"))?;
        let resolution = resolution.ok_or(CommunitySqlError::NullArgument("resolution"))?;
        let seed = seed.ok_or(CommunitySqlError::NullArgument("seed"))?;
        let max_levels = max_levels.ok_or(CommunitySqlError::NullArgument("max_levels"))?;
        let tolerance = tolerance.ok_or(CommunitySqlError::NullArgument("tolerance"))?;
        let randomness = randomness.ok_or(CommunitySqlError::NullArgument("randomness"))?;
        let iterations = iterations.ok_or(CommunitySqlError::NullArgument("iterations"))?;
        if out.starts_with(RESERVED_PREFIX)
            || levels_prefix.is_some_and(|prefix| prefix.starts_with(RESERVED_PREFIX))
        {
            return Err(CommunitySqlError::ReservedForIntermediates);
        }
        let max_levels = at_least_one_level(max_levels)?;
        let iterations = iterations_to_run(iterations)?;
        let tolerance = at_least_zero(tolerance)?;
        let seed = seed as u64;
        let options = Leiden { resolution, seed, max_levels, tolerance, randomness, iterations };
        let graph = graph.ok_or(CommunitySqlError::NullArgument("graph"))?;
        require_resolution(resolution)?;
        let resolved = match pinned {
            Some(pinned) => pinned,
            None => resolve(graph, version, "write vectors of")?,
        };
        let graph_id = resolved.graph.graph_id;
        let _running = Running::start("leiden", Some(graph_id), Some(resolved.version));
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            vectors.check_writable(out, true)?;
            let memory = vectors.memory.clone();
            let loaded = vectors.loaded.clone();
            let mut port = Levels {
                vectors: &mut vectors,
                weighted,
                per_relation,
                graph: graph_id,
                max_levels: options.max_levels,
                prefix: levels_prefix,
            };
            let tombstones = loaded.id_map.tombstones();
            let interrupts = &mut PostgresInterrupts;
            let found = run(&*workers()?, &memory, interrupts, tombstones, options, &mut port)?;
            if !found.stable {
                pgrx::warning!("leiden did not stabilise in {MOST_ITERATIONS} iterations");
            }
            vectors.save(out, found.labels.into(), true)?;
            Ok(Some(found.modularity))
        })
    })
}

/// `max_levels`, refused below 1.
fn at_least_one_level(max_levels: i32) -> Result<u32, CommunitySqlError> {
    u32::try_from(max_levels).ok().filter(|&levels| levels >= 1).ok_or_else(|| {
        CommunitySqlError::InvalidArgument(format!(
            "max_levels must be at least 1, not {max_levels}"
        ))
    })
}

/// `tolerance`, refused if NaN or below 0.
fn at_least_zero(tolerance: f64) -> Result<f64, CommunitySqlError> {
    match tolerance.is_nan() || tolerance < 0.0 {
        true => Err(CommunitySqlError::InvalidArgument(format!(
            "tolerance must be a number of at least 0, not {tolerance}"
        ))),
        false => Ok(tolerance),
    }
}

/// The iterations to run (`None`: until stable, for a negative count),
/// refusing 0.
fn iterations_to_run(iterations: i32) -> Result<Option<u32>, CommunitySqlError> {
    match iterations {
        0 => Err(CommunitySqlError::InvalidArgument(
            "iterations must be at least 1, or negative to run until the partition is stable, \
             not 0"
                .to_owned(),
        )),
        iterations => Ok(u32::try_from(iterations).ok()),
    }
}

/// The port a run reads its version through and reports its levels to.
struct Levels<'a, E> {
    vectors: &'a mut Vectors<E>,
    weighted: Option<bool>,
    per_relation: Option<bool>,
    graph: i32,
    max_levels: u32,
    prefix: Option<&'a str>,
}

impl<E: CachedIds> Multilevel for Levels<'_, E> {
    type Error = CommunitySqlError;

    fn view(&mut self) -> Result<Undirected, CommunitySqlError> {
        undirected(self.vectors, self.weighted, self.per_relation)
    }

    fn level(&mut self, iteration: u32, level: u32) -> Result<(), CommunitySqlError> {
        report_command(Report {
            command: "leiden",
            graph: Some(self.graph),
            phase: &format!("iteration {iteration}, level {level}"),
            done: Some(u64::from(level - 1)),
            total: Some(u64::from(self.max_levels)),
        });
        Ok(())
    }

    fn keeps_levels(&self) -> bool {
        self.prefix.is_some()
    }

    fn keep_level(&mut self, ordinal: u32, labels: Vector<i64>) -> Result<(), CommunitySqlError> {
        if let Some(prefix) = self.prefix {
            self.vectors.save(&format!("{prefix}_{ordinal}"), labels.into(), true)?;
        }
        Ok(())
    }
}
