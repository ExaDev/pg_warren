//! Extension identity: the `warren` schema and `warren.version()`, and the
//! SQLSTATEs of `warren-core`'s graph and compute errors.
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
/// budget, a size limit, an unknown node order, a version to build again
/// (ids in another encoding, or segments of an earlier format), or (when
/// decoding) corrupt data.
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
        GraphError::IdEncoding { .. } | GraphError::SegmentFormat { .. } => {
            PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE
        }
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

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    // `#[pg_test]` functions are not `#[test]` to clippy, so they propagate
    // SPI errors with `?` instead of unwrapping (workspace lints deny unwrap).

    #[test]
    fn a_version_to_build_again_is_not_in_its_prerequisite_state() {
        use super::graph_sqlstate;
        use warren_core::graph::GraphError;
        let again = PgSqlErrorCode::ERRCODE_OBJECT_NOT_IN_PREREQUISITE_STATE;
        assert_eq!(graph_sqlstate(&GraphError::SegmentFormat { found: 1 }), again);
        assert_eq!(graph_sqlstate(&GraphError::IdEncoding { expected: 6, found: 8 }), again);
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
