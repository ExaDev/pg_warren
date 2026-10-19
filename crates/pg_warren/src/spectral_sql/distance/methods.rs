//! The five distance methods (DESIGN.md §8), each as `jsonb` scalars
//! (§14 item 11).

use std::sync::Arc;

use serde_json::{json, Value};
use warren_core::compute::AnyVector;
use warren_core::spectral::{
    degree_distance, edit_distance, embedding_distance, rayleigh_quotients, spectral_distance,
    subspace_distance, ComparedRows, EdgeSetChange, EigenOptions, Resources,
};

use super::super::eigen::eigenpairs;
use super::super::matrix::OperatorArguments;
use super::super::SpectralSqlError;
use super::pair::Pair;
use crate::compute_sql::{workers, Vectors};
use crate::interrupts::PostgresInterrupts;
use crate::source::IdDatum;

/// `edit`: per relation (matched by name) and overall.
pub(super) fn edit<E: IdDatum>(pair: &Pair<E>) -> Value {
    let before = (&pair.before.loaded.relations, &pair.before.loaded.graph);
    let distance = edit_distance(before, pair.after_relations());
    let relations: Vec<Value> = distance
        .relations
        .iter()
        .map(|relation| {
            let mut entry = change_json(&relation.change);
            entry["relation"] = json!(relation.name);
            entry
        })
        .collect();
    json!({ "relations": relations, "overall": change_json(&distance.overall) })
}

fn change_json(change: &EdgeSetChange) -> Value {
    json!({
        "added": change.added, "removed": change.removed,
        "union": change.union, "distance": change.distance,
    })
}

/// `degree`: Wasserstein-1 and KS between the out-degree distributions.
pub(super) fn degree<E: IdDatum>(pair: &Pair<E>) -> Result<Value, SpectralSqlError> {
    let (before, after) = (&pair.before, &pair.after);
    let pool = workers()?;
    let mut resources =
        Resources { workers: &pool, memory: &before.memory, interrupts: &mut PostgresInterrupts };
    let distance = degree_distance(
        &mut resources,
        (&before.loaded.graph, before.tombstones()),
        (&after.loaded.graph, after.tombstones()),
    )?;
    Ok(json!({ "wasserstein": distance.wasserstein, "ks": distance.ks }))
}

/// How the `spectral` method gets each version's eigenvalues.
pub(super) enum Eigenvalues<'a> {
    /// Rayleigh quotients of the stored eigenvectors `<prefix>_1..k`.
    Stored { prefix: &'a str, k: usize },
    /// A fresh solve on each version.
    Solved(EigenOptions),
}

/// `spectral`: the distance between the two eigenvalue sequences.
pub(super) fn spectral<E: IdDatum>(
    pair: &mut Pair<E>,
    operator: &OperatorArguments,
    eigenvalues: &Eigenvalues<'_>,
) -> Result<Value, SpectralSqlError> {
    let before = values(&mut pair.before, operator, eigenvalues)?;
    let after = values(&mut pair.after, operator, eigenvalues)?;
    let distance = spectral_distance(&before, &after);
    Ok(json!({
        "l2": distance.l2, "relative": distance.relative, "max": distance.max,
        "before": before, "after": after,
    }))
}

/// One version's eigenvalues.
fn values<E: IdDatum>(
    vectors: &mut Vectors<E>,
    operator: &OperatorArguments,
    eigenvalues: &Eigenvalues<'_>,
) -> Result<Vec<f64>, SpectralSqlError> {
    match eigenvalues {
        Eigenvalues::Solved(options) => {
            Ok(eigenpairs(vectors, operator, options)?.pairs.iter().map(|p| p.value).collect())
        }
        Eigenvalues::Stored { prefix, k } => {
            let stored = (1..=*k)
                .map(|i| vectors.get(&format!("{prefix}_{i}")))
                .collect::<Result<Vec<_>, _>>()?;
            let loaded = Arc::clone(&vectors.loaded);
            let selected = operator.relations(&loaded.relations);
            let matrix = operator.matrix(selected.as_deref())?;
            let pool = workers()?;
            let memory = &vectors.memory;
            let mut resources =
                Resources { workers: &pool, memory, interrupts: &mut PostgresInterrupts };
            let refs: Vec<&AnyVector> = stored.iter().map(|v| &**v).collect();
            Ok(rayleigh_quotients(&mut resources, &loaded.graph, &matrix, &refs)?)
        }
    }
}

/// `subspace`: principal angles between the spans of `<prefix>_1..k` of
/// each version.
pub(super) fn subspace<E: IdDatum>(
    pair: &mut Pair<E>,
    prefix: &str,
    k: usize,
    compared: ComparedRows,
) -> Result<Value, SpectralSqlError> {
    let names: Vec<String> = (1..=k).map(|i| format!("{prefix}_{i}")).collect();
    let before = names.iter().map(|n| pair.before_vector(n)).collect::<Result<Vec<_>, _>>()?;
    let after = names.iter().map(|n| pair.after_vector(n)).collect::<Result<Vec<_>, _>>()?;
    let (before, after): (Vec<&AnyVector>, Vec<&AnyVector>) =
        (before.iter().map(|v| &**v).collect(), after.iter().map(|v| &**v).collect());
    let pool = workers()?;
    let memory = &pair.before.memory;
    let mut resources = Resources { workers: &pool, memory, interrupts: &mut PostgresInterrupts };
    let distance = subspace_distance(&mut resources, &before, &after, compared)?;
    Ok(json!({
        "projection": distance.projection, "max_angle": distance.max_angle,
        "angles": distance.angles,
    }))
}

/// `embedding`: row-wise distances between matrix `x` of the earlier
/// version and matrix `y` of the later.
pub(super) fn embedding<E: IdDatum>(
    pair: &mut Pair<E>,
    x: &str,
    y: &str,
    compared: ComparedRows,
) -> Result<Value, SpectralSqlError> {
    let (before, after) = (pair.before_vector(x)?, pair.after_vector(y)?);
    let pool = workers()?;
    let memory = &pair.before.memory;
    let mut resources = Resources { workers: &pool, memory, interrupts: &mut PostgresInterrupts };
    let distance = embedding_distance(&mut resources, &before, &after, compared)?;
    Ok(json!({
        "mean_l2": distance.mean_l2, "max_l2": distance.max_l2,
        "mean_cosine": distance.mean_cosine, "max_cosine": distance.max_cosine,
        "rows": distance.rows,
    }))
}
