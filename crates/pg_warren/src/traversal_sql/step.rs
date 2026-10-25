//! `warren.step`, the stable intermediate representation of a walk's steps
//! (DESIGN.md §9.2, §14 item 14): the composite type, its `warren.s()`
//! constructor, the `jsonb → warren.step[]` cast (which refuses anything
//! but an array of objects with the five keys, `rels` and `filter` arrays
//! or null), and reading steps into
//! `warren-core`'s [`Step`]s against a loaded version.
//!
//! A composite type cannot have a polymorphic attribute, so a step holds
//! its filter's external ids as `text`, in their id type's output format;
//! `warren.walk` reads them back with the graph's id type's input function.

use std::borrow::Borrow;

use pgrx::heap_tuple::PgHeapTuple;
use pgrx::prelude::*;
use pgrx::{AllocatedByRust, AnyArray, Array};
use warren_core::traversal::{HopRange, NodeSet, Step};

use super::ids::parse;
use super::{entry, TraversalSqlError};
use crate::cache::{CachedIds, LoadedVersion};
use crate::external_ids::{element_type, with_datums};
use crate::selection::{direction, relations};

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
IMMUTABLE STRICT PARALLEL SAFE LANGUAGE plpgsql
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    step jsonb;
    position int := 0;
    key text;
    result warren.step[] := '{}';
BEGIN
    IF jsonb_typeof(steps) <> 'array' THEN
        RAISE EXCEPTION USING ERRCODE = 'invalid_parameter_value',
            MESSAGE = format('steps must be a JSON array of step objects, not %s',
                             jsonb_typeof(steps));
    END IF;
    FOR step IN SELECT e FROM jsonb_array_elements(steps) AS x(e) LOOP
        position := position + 1;
        IF jsonb_typeof(step) <> 'object' THEN
            RAISE EXCEPTION USING ERRCODE = 'invalid_parameter_value',
                MESSAGE = format('step %s must be an object, not %s', position,
                                 jsonb_typeof(step));
        END IF;
        FOR key IN SELECT jsonb_object_keys(step) LOOP
            IF key NOT IN ('rels', 'direction', 'filter', 'min_hops', 'max_hops') THEN
                RAISE EXCEPTION USING ERRCODE = 'invalid_parameter_value',
                    MESSAGE = format('step %s has an unknown key "%s"', position, key);
            END IF;
        END LOOP;
        FOREACH key IN ARRAY ARRAY['rels', 'filter'] LOOP
            IF jsonb_typeof(step -> key) NOT IN ('array', 'null') THEN
                RAISE EXCEPTION USING ERRCODE = 'invalid_parameter_value',
                    MESSAGE = format('step %s: %s must be an array or null, not %s', position,
                                     key, jsonb_typeof(step -> key));
            END IF;
        END LOOP;
        result := result || ROW(
            CASE WHEN jsonb_typeof(step -> 'rels') = 'array' THEN
                ARRAY(SELECT jsonb_array_elements_text(step -> 'rels')) END,
            coalesce(step ->> 'direction', 'out'),
            CASE WHEN jsonb_typeof(step -> 'filter') = 'array' THEN
                ARRAY(SELECT jsonb_array_elements_text(step -> 'filter')) END,
            coalesce((step ->> 'min_hops')::int, 1),
            (step ->> 'max_hops')::int)::warren.step;
    END LOOP;
    RETURN result;
END
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
        let filter = match filter {
            Some(filter) => Some(texts(&filter)?),
            None => None,
        };
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
fn texts(array: &AnyArray) -> Result<Vec<Option<String>>, TraversalSqlError> {
    let element = element_type("filter", array.oid(), "bigint")?;
    let mut output = pg_sys::Oid::INVALID;
    let mut varlena = false;
    // SAFETY: the out-pointers point to live locals; an unknown type raises
    // an ERROR.
    unsafe { pg_sys::getTypeOutputInfo(element, &mut output, &mut varlena) };
    Ok(with_datums(array, |datums| {
        datums
            .map(|value| {
                value.map(|datum| {
                    // SAFETY: `output` is the element type's output function
                    // and the datum is an element of that type, alive while
                    // `with_datums` reads; it returns a palloc'd
                    // NUL-terminated string, copied out here.
                    let text = unsafe { pg_sys::OidOutputFunctionCall(output, datum) };
                    // SAFETY: as above.
                    unsafe { std::ffi::CStr::from_ptr(text) }.to_string_lossy().into_owned()
                })
            })
            .collect()
    }))
}

/// The steps of a walk, read into `warren-core` steps over `loaded`.
pub(crate) fn read_steps<E: CachedIds>(
    loaded: &LoadedVersion<E>,
    steps: &Array<'_, PgHeapTuple<'_, AllocatedByRust>>,
) -> Result<Vec<Step>, TraversalSqlError> {
    steps
        .iter()
        .map(|step| step.ok_or(TraversalSqlError::NullArgument("a step")))
        .map(|step| read_step(loaded, &step?))
        .collect()
}

fn read_step<E: CachedIds>(
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
fn filter_set<E: CachedIds>(
    loaded: &LoadedVersion<E>,
    members: &[Option<String>],
) -> Result<NodeSet, TraversalSqlError> {
    let ids =
        members.iter().flatten().map(|member| parse::<E>(member)).collect::<Result<Vec<_>, _>>()?;
    let dense = ids.iter().filter_map(|id| loaded.id_map.dense_id(id.borrow()));
    Ok(NodeSet::from_dense_ids(loaded.id_map.id_space(), dense)?)
}
