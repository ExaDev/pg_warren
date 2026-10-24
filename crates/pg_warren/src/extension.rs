//! Extension identity: the `warren` schema and `warren.version()`, the
//! SQLSTATEs of `warren-core`'s graph and compute errors, and arguments
//! added since a release.
//!
//! The schema is declared once, here, as `#[pg_schema] mod warren`; pgrx emits
//! `CREATE SCHEMA IF NOT EXISTS warren` into the extension script, so the
//! schema is a member of the extension and is dropped with it. See the note in
//! `pg_warren.control` for why the control file has no `schema` key. Feature
//! modules place SQL functions in it with `#[pg_extern(schema = "warren")]`.

use pgrx::prelude::*;

use crate::access::{entry, AccessError};
use warren_core::compute::{ArithmeticError, ComputeError};
use warren_core::graph::GraphError;

/// The `warren` SQL schema. Intentionally empty: it exists so pgrx creates
/// the schema; functions live in their feature modules.
#[pg_schema]
mod warren {}

/// The SQLSTATE of a graph error: out of memory, over the build memory
/// budget, a size limit, an unknown node order, or (when decoding) corrupt
/// data.
pub(crate) fn graph_sqlstate(error: &GraphError) -> PgSqlErrorCode {
    match error {
        GraphError::OutOfMemory { .. } => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
        GraphError::OffsetsOverBudget { .. } | GraphError::BuildMemoryLimit { .. } => {
            PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED
        }
        GraphError::TooManyRelations
        | GraphError::IdSpaceTooLarge
        | GraphError::RelationNameTooLong => PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED,
        GraphError::UnknownNodeOrder(_) => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        GraphError::IdEncoding { .. } => PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE,
        _ => PgSqlErrorCode::ERRCODE_DATA_CORRUPTED,
    }
}

/// The SQLSTATE of a kernel error: the one Postgres raises for the same
/// arithmetic, so the compiled path and the executor agree.
pub(crate) fn compute_sqlstate(error: &ComputeError) -> PgSqlErrorCode {
    match error {
        ComputeError::MemoryLimit { .. } => PgSqlErrorCode::ERRCODE_CONFIGURATION_LIMIT_EXCEEDED,
        ComputeError::OutOfMemory { .. } => PgSqlErrorCode::ERRCODE_OUT_OF_MEMORY,
        ComputeError::UnknownDtype(_)
        | ComputeError::UnknownSemiring(_)
        | ComputeError::UnknownBinaryOp(_)
        | ComputeError::UnknownReduction(_)
        | ComputeError::UnknownNorm(_)
        | ComputeError::UnknownCombiner(_)
        | ComputeError::UnknownSimilarity(_)
        | ComputeError::ZeroWidth
        | ComputeError::WidthMismatch { .. }
        | ComputeError::Cycle { .. }
        | ComputeError::NonPositiveWeight => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
        ComputeError::DtypeMismatch { .. }
        | ComputeError::OperandType { .. }
        | ComputeError::CombinerType { .. } => PgSqlErrorCode::ERRCODE_DATATYPE_MISMATCH,
        ComputeError::IdSpaceMismatch { .. } => {
            PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
        }
        ComputeError::NullResult { .. } => PgSqlErrorCode::ERRCODE_NULL_VALUE_NOT_ALLOWED,
        ComputeError::Arithmetic(error) => arithmetic_sqlstate(*error),
        ComputeError::Cancelled => PgSqlErrorCode::ERRCODE_QUERY_CANCELED,
        ComputeError::VectorBytes { .. } | ComputeError::VectorLength { .. } => {
            PgSqlErrorCode::ERRCODE_DATA_CORRUPTED
        }
        _ => PgSqlErrorCode::ERRCODE_INTERNAL_ERROR,
    }
}

/// The SQLSTATE `float.c` and `int8.c` raise for an arithmetic error.
fn arithmetic_sqlstate(error: ArithmeticError) -> PgSqlErrorCode {
    match error {
        ArithmeticError::BigintOutOfRange
        | ArithmeticError::FloatOverflow
        | ArithmeticError::FloatUnderflow => PgSqlErrorCode::ERRCODE_NUMERIC_VALUE_OUT_OF_RANGE,
        ArithmeticError::DivisionByZero => PgSqlErrorCode::ERRCODE_DIVISION_BY_ZERO,
        ArithmeticError::SqrtOfNegative
        | ArithmeticError::ZeroToNegativePower
        | ArithmeticError::ComplexPower => {
            PgSqlErrorCode::ERRCODE_INVALID_ARGUMENT_FOR_POWER_FUNCTION
        }
        ArithmeticError::LogOfZero | ArithmeticError::LogOfNegative => {
            PgSqlErrorCode::ERRCODE_INVALID_ARGUMENT_FOR_LOG
        }
    }
}

/// `warren.version()`: the version of the installed extension library.
#[pg_extern(schema = "warren", stable, parallel_safe)]
pub fn version() -> &'static str {
    entry(|_| Ok::<_, AccessError>(env!("CARGO_PKG_VERSION")))
}

/// A `boolean` argument added after a release, at `position` (from 0),
/// read from `fcinfo` rather than as a parameter: between installing this
/// library and `ALTER EXTENSION pg_warren UPDATE`, the release's definition,
/// which does not pass it, still calls the function, and pgrx would refuse
/// the missing argument. Absent, it is `false` (its default); NULL is
/// `None`.
pub(crate) fn added_flag(fcinfo: pg_sys::FunctionCallInfo, position: usize) -> Option<bool> {
    // SAFETY: fcinfo is this call's, and holds `nargs` arguments.
    let passed = usize::try_from(unsafe { (*fcinfo).nargs }).is_ok_and(|nargs| nargs > position);
    match passed {
        // SAFETY: fcinfo is this call's, the argument exists, and every
        // definition that passes it declares it `boolean`.
        true => unsafe { pgrx::fcinfo::pg_getarg::<bool>(fcinfo, position) },
        false => Some(false),
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{rows, small};

    // `#[pg_test]` functions are not `#[test]` to clippy, so they propagate
    // SPI errors with `?` instead of unwrapping (workspace lints deny unwrap).

    /// The 0.6.0 install script.
    const RELEASED: &str = include_str!("../../../sql/pg_warren--0.6.0.sql");

    /// Each C function given an argument since 0.6.0, and a call of it over
    /// the small graph with `{s}` its schema, whose result (as text) does
    /// not depend on the schema.
    const ADDED_SINCE: &[(&str, &str)] = &[
        ("modularity", "{s}.modularity('t', 'lab')"),
        ("contract", "{s}.contract('t', 'lab', 'q_{s}') IS NOT NULL"),
        ("local_moving", "{s}.local_moving('t', 'lm_{s}', persist := false)"),
        ("leiden_refine", "{s}.leiden_refine('t', 'lab', 'lr_{s}', persist := false)"),
        ("spmv", "{s}.spmv('t', 'one', 'v_{s}', persist := false)"),
        ("spmm", "{s}.spmm('t', 'one', 'm_{s}', persist := false)"),
        ("vector_degree", "{s}.vector_degree('t', 'd_{s}', persist := false)"),
        ("degree", "{s}.degree('t', 1::bigint)"),
        ("eigen", "(SELECT string_agg(round(eigenvalue::numeric, 6)::text, ' ' ORDER BY i)
                     FROM {s}.eigen('t', 2, dtype := 'f64'))"),
        (
            "distance",
            "{s}.distance('t', (warren.info('t') ->> 'current_version')::bigint,
                          (warren.info('t') ->> 'current_version')::bigint, 'degree') ->> 'ks'",
        ),
        (
            "gather",
            "(SELECT string_agg(node_id || ':' || value, ' ' ORDER BY node_id)
              FROM {s}.gather('t', 'SELECT id, id FROM t_nodes', NULL::bigint, NULL::bigint, 'sum'))",
        ),
        ("dag_longest_path", "{s}.dag_longest_path('t', 'dl_{s}', ARRAY['a'], persist := false)"),
        (
            "minimum_spanning_forest",
            "(SELECT string_agg(source || '-' || target || ':' || weight, ' ')
              FROM {s}.minimum_spanning_forest('t', NULL::bigint))",
        ),
        ("betweenness_centrality", "{s}.betweenness_centrality('t', 'bc_{s}', persist := false)"),
        ("closeness_centrality", "{s}.closeness_centrality('t', 'cc_{s}', persist := false)"),
        ("harmonic_centrality", "{s}.harmonic_centrality('t', 'hc_{s}', persist := false)"),
        ("dijkstra", "(SELECT cost FROM {s}.dijkstra('t', 1::bigint, 4::bigint))"),
    ];

    /// `function`'s definition in the 0.6.0 install script, in schema
    /// `released`, calling this library.
    fn released(function: &str) -> String {
        let starts = [
            format!("CREATE  FUNCTION warren.\"{function}\"("),
            format!("CREATE FUNCTION warren.{function}("),
        ];
        let start = starts.iter().find_map(|start| RELEASED.find(start.as_str())).unwrap_or(0);
        let end = RELEASED[start..].find("_wrapper';").map_or(0, |end| start + end);
        RELEASED[start..end]
            .replacen("warren.", "released.", 1)
            .replace("'MODULE_PATHNAME'", "'pg_warren'")
            + "_wrapper'"
    }

    /// Between installing this library and `ALTER EXTENSION pg_warren
    /// UPDATE`, the released definitions call it: an argument added since
    /// then is absent from their calls, and takes its default.
    #[pg_test]
    fn released_definitions_call_this_library() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE SCHEMA released;
             SELECT warren.vector_dense_id('t', 'lab', 'f64');
             SELECT warren.vector_const('t', 'one', 1)",
        )?;
        for &(function, call) in ADDED_SINCE {
            Spi::run(&released(function))?;
            let through = |schema| rows(&format!("SELECT ({})::text", call.replace("{s}", schema)));
            assert_eq!(through("released")?, through("warren")?, "{function}");
        }
        Ok(())
    }

    #[pg_test]
    fn warren_version_returns_crate_version() -> Result<(), Error> {
        let v = Spi::get_one::<String>("SELECT warren.version()")?;
        assert_eq!(v.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        Ok(())
    }

    #[pg_test]
    fn warren_version_lives_in_warren_schema() -> Result<(), Error> {
        let in_warren = Spi::get_one::<bool>(
            "SELECT pronamespace = 'warren'::regnamespace
             FROM pg_proc WHERE oid = 'warren.version()'::regprocedure",
        )?;
        assert_eq!(in_warren, Some(true));
        Ok(())
    }

    #[pg_test]
    fn warren_version_is_stable_not_immutable() -> Result<(), Error> {
        // The answer changes when the library is upgraded in place, so it must
        // not be constant-folded into stored plans or index expressions.
        let volatility = Spi::get_one::<String>(
            "SELECT provolatile::text FROM pg_proc
             WHERE oid = 'warren.version()'::regprocedure",
        )?;
        assert_eq!(volatility.as_deref(), Some("s"));
        Ok(())
    }

    #[pg_test]
    fn extension_creates_warren_schema() -> Result<(), Error> {
        let owns = Spi::get_one::<bool>(
            "SELECT EXISTS (
                 SELECT 1 FROM pg_depend d
                 JOIN pg_extension e ON e.oid = d.refobjid
                 JOIN pg_namespace n ON n.oid = d.objid
                 WHERE e.extname = 'pg_warren' AND n.nspname = 'warren'
                   AND d.classid = 'pg_namespace'::regclass)",
        )?;
        assert_eq!(owns, Some(true), "schema warren must be created by the extension");
        Ok(())
    }

    #[pg_test]
    fn drop_extension_drops_warren_schema() -> Result<(), Error> {
        // Other tests run concurrently and lock the catalog tables in this
        // order; DROP EXTENSION would take them in its own order and could
        // deadlock with them, so take them first, in theirs.
        Spi::run(
            "LOCK TABLE warren.graphs, warren.versions, warren.segments, warren.vectors,
                        warren.vector_chunks IN ACCESS EXCLUSIVE MODE;
             DROP EXTENSION pg_warren CASCADE",
        )?;
        let exists = Spi::get_one::<bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_namespace WHERE nspname = 'warren')",
        )?;
        assert_eq!(exists, Some(false));
        Ok(())
    }
}
