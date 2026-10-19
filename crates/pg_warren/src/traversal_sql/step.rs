//! `warren.step`, the stable intermediate representation of a walk's steps
//! (DESIGN.md §9.2, §14 item 14): the composite type, its `warren.s()`
//! constructor, the `jsonb → warren.step[]` cast, and reading steps into
//! `warren-core`'s [`Step`]s against a loaded version.
//!
//! A composite type cannot have a polymorphic attribute, so a step holds
//! its filter's external ids as `text`, in their id type's output format;
//! `warren.walk` reads them back with the graph's id type's input function.

use pgrx::heap_tuple::PgHeapTuple;
use pgrx::prelude::*;
use pgrx::{AllocatedByRust, AnyArray, Array};
use warren_core::traversal::{HopRange, NodeSet, Step};

use super::args::{direction, relations};
use super::ids::parse;
use super::{entry, TraversalSqlError};
use crate::cache::LoadedVersion;
use crate::source::IdDatum;

pgrx::extension_sql!(
    r#"
CREATE TYPE warren.step AS (
    rels      text[],
    direction text,
    filter    text[],
    min_hops  int,
    max_hops  int
);

CREATE FUNCTION warren.steps(steps jsonb) RETURNS warren.step[]
IMMUTABLE STRICT PARALLEL SAFE LANGUAGE sql
SET search_path = pg_catalog, pg_temp
AS $$
    SELECT coalesce(array_agg(ROW(
               CASE WHEN jsonb_typeof(e -> 'rels') = 'array' THEN
                   ARRAY(SELECT jsonb_array_elements_text(e -> 'rels')) END,
               coalesce(e ->> 'direction', 'out'),
               CASE WHEN jsonb_typeof(e -> 'filter') = 'array' THEN
                   ARRAY(SELECT jsonb_array_elements_text(e -> 'filter')) END,
               coalesce((e ->> 'min_hops')::int, 1),
               (e ->> 'max_hops')::int)::warren.step ORDER BY o), '{}')
    FROM jsonb_array_elements(steps) WITH ORDINALITY AS x(e, o)
$$;

CREATE CAST (jsonb AS warren.step[]) WITH FUNCTION warren.steps(jsonb);
"#,
    name = "walk_step",
    requires = [extension::warren],
);

/// `warren.s`: a step that follows `rels` (every relation if NULL) in
/// `direction` for `min_hops` to `max_hops` hops (`max_hops` NULL: exactly
/// `min_hops`), entering only nodes in `filter` (any node if NULL). The
/// filter's ids are kept as text; `warren.walk` checks them against the
/// graph.
#[pg_extern(
    schema = "warren",
    requires = ["walk_step"],
    sql = r#"
CREATE FUNCTION warren.s(rels text[], direction text DEFAULT 'out', filter "any" DEFAULT NULL,
                         min_hops int DEFAULT 1, max_hops int DEFAULT NULL)
RETURNS warren.step
STABLE PARALLEL SAFE LANGUAGE c AS 'MODULE_PATHNAME', 's_wrapper';
"#
)]
fn s(
    rels: Option<Vec<Option<String>>>,
    direction: Option<&str>,
    filter: Option<AnyArray>,
    min_hops: Option<i32>,
    max_hops: Option<i32>,
) -> pgrx::composite_type!('static, "warren.step") {
    entry(|| {
        let mut step = PgHeapTuple::new_composite_type("warren.step")
            .map_err(|error| TraversalSqlError::Step(error.to_string()))?;
        let filter = filter.map(|filter| texts(&filter));
        let fields = [
            step.set_by_name("rels", rels),
            step.set_by_name("direction", direction.map(str::to_owned)),
            step.set_by_name("filter", filter),
            step.set_by_name("min_hops", min_hops),
            step.set_by_name("max_hops", max_hops),
        ];
        for field in fields {
            field.map_err(|error| TraversalSqlError::Step(error.to_string()))?;
        }
        Ok(step)
    })
}

/// The elements of `array` as text, in their type's output format.
fn texts(array: &AnyArray) -> Vec<Option<String>> {
    // SAFETY: get_element_type only reads the type cache.
    let element = unsafe { pg_sys::get_element_type(array.oid()) };
    let mut output = pg_sys::Oid::INVALID;
    let mut varlena = false;
    // SAFETY: the out-pointers point to live locals; an unknown type raises
    // an ERROR.
    unsafe { pg_sys::getTypeOutputInfo(element, &mut output, &mut varlena) };
    array
        .into_iter()
        .map(|value| {
            value.map(|value| {
                // SAFETY: `output` is the element type's output function and
                // the datum is an element of that type; it returns a palloc'd
                // NUL-terminated string.
                let text = unsafe { pg_sys::OidOutputFunctionCall(output, value.datum()) };
                // SAFETY: as above.
                unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned()
            })
        })
        .collect()
}

/// The steps of a walk, read into `warren-core` steps over `loaded`.
pub(crate) fn read_steps<E: IdDatum>(
    loaded: &LoadedVersion<E>,
    steps: &Array<'_, PgHeapTuple<'_, AllocatedByRust>>,
) -> Result<Vec<Step>, TraversalSqlError> {
    steps
        .iter()
        .map(|step| step.ok_or(TraversalSqlError::NullArgument("a step")))
        .map(|step| read_step(loaded, &step?))
        .collect()
}

fn read_step<E: IdDatum>(
    loaded: &LoadedVersion<E>,
    step: &PgHeapTuple<'_, AllocatedByRust>,
) -> Result<Step, TraversalSqlError> {
    let field_error =
        |error: pgrx::datum::TryFromDatumError| TraversalSqlError::Step(error.to_string());
    let rels: Option<Vec<Option<String>>> = step
        .get_by_name::<Array<'_, String>>("rels")
        .map_err(field_error)?
        .map(|rels| rels.iter().collect());
    let direction_text: Option<String> = step.get_by_name("direction").map_err(field_error)?;
    let filter: Option<Vec<Option<String>>> = step
        .get_by_name::<Array<'_, String>>("filter")
        .map_err(field_error)?
        .map(|filter| filter.iter().collect());
    let min_hops: i32 = step.get_by_name("min_hops").map_err(field_error)?.unwrap_or(1);
    let max_hops: Option<i32> = step.get_by_name("max_hops").map_err(field_error)?;
    let min = u32::try_from(min_hops).map_err(|_| TraversalSqlError::Negative("min_hops"))?;
    let max = match max_hops {
        Some(max) => u32::try_from(max).map_err(|_| TraversalSqlError::Negative("max_hops"))?,
        None => min,
    };
    let filter = match filter {
        Some(members) => Some(filter_set(loaded, &members)?),
        None => None,
    };
    Ok(Step {
        relations: relations(&loaded.relations, rels.as_deref()),
        direction: direction(Some(direction_text.as_deref().unwrap_or("out")))?,
        filter,
        hops: HopRange::new(min, max)?,
    })
}

/// The live nodes among a step filter's external ids, as text.
fn filter_set<E: IdDatum>(
    loaded: &LoadedVersion<E>,
    members: &[Option<String>],
) -> Result<NodeSet, TraversalSqlError> {
    let ids =
        members.iter().flatten().map(|member| parse::<E>(member)).collect::<Result<Vec<E>, _>>()?;
    let dense = ids.iter().filter_map(|id| loaded.id_map.dense_id(id));
    Ok(NodeSet::from_dense_ids(loaded.id_map.id_space(), dense)?)
}
