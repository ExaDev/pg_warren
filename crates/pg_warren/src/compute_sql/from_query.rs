//! `warren.vector_from_query` (DESIGN.md §6.2): a vector materialised from
//! a query's `(id, value)` rows, through the version's id map.
//!
//! The query runs as the caller, under the caller's `search_path`, as a
//! read-only cursor. It is first prepared without being planned or run, to
//! read its column types: it must be one SELECT (or VALUES, or TABLE) of
//! two columns whose first casts implicitly to the graph's id type (so ids
//! are never rounded: a `double precision` id is an error). Its values are
//! cast to the dtype's SQL type by Postgres itself, as `::` casts
//! (`double precision`, `real`, `bigint`, `boolean`, or an array of it for
//! a matrix row: `2.5` is 3 in an `i64` vector), and its ids to the graph's
//! id type. Nodes
//! without a row get `fill`; ids that are NULL, unknown or tombstoned in
//! the version are skipped; a node with two rows is an error. A NULL value
//! is stored as SQL NULL is everywhere in vectors: `NaN` in a float vector,
//! an error in an `i64` or `bool` vector.

use std::ffi::CStr;

use pgrx::prelude::*;
use pgrx::spi::SpiHeapTupleData;
use pgrx::{FromDatum, IntoDatum};
use warren_core::compute::{map, AnyVector, ComputeError, Dtype, MemoryCharge, Value, Vector};

use super::constructors::{dtype as parse_dtype, literal};
use super::settings::workers;
use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::access::{with_read_only, with_search_path, Caller};
use crate::cache::resolve;
use crate::interrupts::PostgresInterrupts;
use crate::source::{
    describe, for_each_row, open_read_only, type_name, with_id_type, ColumnType, IdDatum,
    SourceError,
};

/// The SQL type a dtype's values are cast to.
fn sql_type(dtype: Dtype) -> &'static str {
    match dtype {
        Dtype::F64 => "pg_catalog.float8",
        Dtype::F32 => "pg_catalog.float4",
        Dtype::I64 => "pg_catalog.int8",
        Dtype::Bool => "pg_catalog.bool",
    }
}

/// `warren.vector_from_query`: vector `name` of `dtype` from `query`'s
/// `(id, value)` rows (`value` an array for a matrix), bound to `version`
/// (the current version if NULL). Returns its vector id (NULL if not
/// `persist`: a transient vector).
#[pg_extern(schema = "warren", volatile, parallel_unsafe)]
fn vector_from_query(
    graph: Option<&str>,
    name: Option<&str>,
    query: Option<&str>,
    dtype: default!(Option<&str>, "'f64'"),
    fill: default!(Option<f64>, "0"),
    persist: default!(Option<bool>, "true"),
    version: default!(Option<i64>, "NULL"),
) -> Option<i64> {
    entry(|caller| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        let name = name.ok_or(ComputeSqlError::NullArgument("name"))?;
        let persist = persist.ok_or(ComputeSqlError::NullArgument("persist"))?;
        let query = query.ok_or(ComputeSqlError::NullArgument("query"))?;
        let dtype = parse_dtype(dtype)?;
        let fill = fill.ok_or(ComputeSqlError::NullArgument("fill"))?;
        let resolved = resolve(graph, version, "write vectors of")?;
        with_id_type!(resolved.graph.id_type, E => {
            let mut vectors = Vectors::<E>::open(resolved)?;
            let read = Read { caller, query, fill, id_type: id_type_name::<E>() };
            let vector = match dtype {
                Dtype::F64 => read.vector::<E, f64>(&vectors)?,
                Dtype::F32 => read.vector::<E, f32>(&vectors)?,
                Dtype::I64 => read.vector::<E, i64>(&vectors)?,
                Dtype::Bool => read.vector::<E, bool>(&vectors)?,
            };
            vectors.save(name, vector, persist)
        })
    })
}

/// Checks that a query's id column casts implicitly to the graph's id
/// type `E`, so ids are never rounded.
pub(super) fn check_id_column<E: IdDatum>(column: &ColumnType) -> Result<(), ComputeSqlError> {
    let id_type = E::Sql::type_oid();
    // SAFETY: can_coerce_type reads one type pair from the catalogs.
    let implicit = unsafe {
        pg_sys::can_coerce_type(
            1,
            &column.oid,
            &id_type,
            pg_sys::CoercionContext::COERCION_IMPLICIT,
        )
    };
    match implicit {
        true => Ok(()),
        false => Err(ComputeSqlError::IdColumnType {
            found: column.name.clone(),
            id_type: type_name(id_type),
        }),
    }
}

/// The qualified SQL name of `E`'s type.
pub(super) fn id_type_name<E: IdDatum>() -> String {
    // SAFETY: format_type_be_qualified returns a palloc'd NUL-terminated
    // string for any type oid.
    let name = unsafe { CStr::from_ptr(pg_sys::format_type_be_qualified(E::Sql::type_oid())) };
    name.to_string_lossy().into_owned()
}

/// The external id `id` as text, by its type's output function.
fn id_text<E: IdDatum>(id: E) -> String {
    let Some(datum) = id.to_sql().into_datum() else {
        return String::new();
    };
    let (mut output, mut varlena) = (pg_sys::Oid::INVALID, false);
    // SAFETY: the out-pointers point to live locals; the datum is of the
    // type whose output function is called, which returns a palloc'd
    // NUL-terminated string.
    unsafe {
        pg_sys::getTypeOutputInfo(E::Sql::type_oid(), &mut output, &mut varlena);
        CStr::from_ptr(pg_sys::OidOutputFunctionCall(output, datum)).to_string_lossy().into_owned()
    }
}

/// A query to read a vector from.
struct Read<'a> {
    caller: &'a Caller,
    query: &'a str,
    fill: f64,
    id_type: String,
}

impl Read<'_> {
    /// The vector of `T` the query's rows make.
    fn vector<E: IdDatum, T: Value + FromDatum + IntoDatum>(
        &self,
        vectors: &Vectors<E>,
    ) -> Result<AnyVector, ComputeSqlError>
    where
        AnyVector: From<Vector<T>>,
        Vec<Option<T>>: FromDatum,
    {
        let query = self.query.trim_end().trim_end_matches(';');
        with_search_path(self.caller.search_path(), || {
            with_read_only(|| {
                Spi::connect(|client| {
                    let columns = describe::<ComputeSqlError>("query", query)?;
                    if columns.len() != 2 {
                        return Err(SourceError::ColumnCount {
                            query: "query",
                            expected: "2 (id, value)",
                            found: columns.len(),
                        }
                        .into());
                    }
                    check_id_column::<E>(&columns[0])?;
                    // SAFETY: get_element_type only reads the type cache.
                    let array =
                        unsafe { pg_sys::get_element_type(columns[1].oid) } != pg_sys::Oid::INVALID;
                    let cast = format!("{}{}", sql_type(T::DTYPE), if array { "[]" } else { "" });
                    let wrapped = format!(
                        "SELECT q.id::{}, q.value::{cast} FROM ({query}\n) AS q(id, value)",
                        self.id_type
                    );
                    let mut opened = open_read_only::<ComputeSqlError>(client, "query", &wrapped)?;
                    let mut rows = Rows::<T>::new(vectors, self.fill_value::<E, T>(vectors)?)?;
                    for_each_row(&mut opened.cursor, |row| match array {
                        true => rows.matrix_row(vectors, row),
                        false => rows.scalar_row(vectors, row),
                    })?;
                    rows.finish(vectors)
                })
            })
        })
    }

    /// `fill` cast to `T` as SQL casts `double precision`.
    fn fill_value<E: IdDatum, T: Value>(&self, vectors: &Vectors<E>) -> Result<T, ComputeSqlError> {
        let one = map(
            &*workers()?,
            &vectors.memory,
            &mut PostgresInterrupts,
            1,
            &literal(self.fill, T::DTYPE),
            &[],
        )?;
        Ok(T::vector(&one).map_or_else(T::default, |vector| vector.values()[0]))
    }
}

/// The rows read so far: the vector, allocated once the first row gives
/// its width, and which nodes have had a row.
struct Rows<T> {
    fill: T,
    values: Option<Vector<T>>,
    seen: Vec<bool>,
    _seen: MemoryCharge,
}

impl<T: Value + FromDatum + IntoDatum> Rows<T>
where
    AnyVector: From<Vector<T>>,
    Vec<Option<T>>: FromDatum,
{
    fn new<E: IdDatum>(vectors: &Vectors<E>, fill: T) -> Result<Self, ComputeSqlError> {
        let id_space = vectors.id_space();
        let charge = vectors.memory.charge(id_space)?;
        let mut seen = Vec::new();
        seen.try_reserve_exact(id_space as usize)
            .map_err(|_| ComputeError::OutOfMemory { bytes: charge.bytes() })?;
        seen.resize(id_space as usize, false);
        Ok(Self { fill, values: None, seen, _seen: charge })
    }

    /// The dense id of the row's node, if it is a live node of the version
    /// and has had no row before.
    fn node<E: IdDatum>(
        &mut self,
        vectors: &Vectors<E>,
        row: &SpiHeapTupleData<'_>,
    ) -> Result<Option<usize>, ComputeSqlError> {
        let Some(id) = row.get::<E::Sql>(1)?.map(E::from_sql) else {
            return Ok(None);
        };
        let Some(node) = vectors.loaded.id_map.dense_id(&id) else {
            return Ok(None);
        };
        let node = node as usize;
        if std::mem::replace(&mut self.seen[node], true) {
            return Err(ComputeSqlError::DuplicateNode(id_text(id)));
        }
        Ok(Some(node))
    }

    /// The vector, allocated at `width` on first use.
    fn values<E: IdDatum>(
        &mut self,
        vectors: &Vectors<E>,
        width: usize,
    ) -> Result<&mut Vector<T>, ComputeSqlError> {
        let values = match self.values.take() {
            Some(values) => values,
            None => Vector::filled(&vectors.memory, vectors.id_space(), width, self.fill)?,
        };
        let earlier = values.width();
        let values = self.values.insert(values);
        if earlier != width {
            return Err(ComputeSqlError::InvalidArgument(format!(
                "query rows have {width} values where earlier rows had {earlier}"
            )));
        }
        Ok(values)
    }

    /// A NULL value as stored.
    fn null() -> Result<T, ComputeSqlError> {
        Ok(T::NULL.ok_or(ComputeError::NullResult { dtype: T::DTYPE })?)
    }

    fn scalar_row<E: IdDatum>(
        &mut self,
        vectors: &Vectors<E>,
        row: &SpiHeapTupleData<'_>,
    ) -> Result<(), ComputeSqlError> {
        let Some(node) = self.node(vectors, row)? else {
            return Ok(());
        };
        let value = match row.get::<T>(2)? {
            Some(value) => value,
            None => Self::null()?,
        };
        self.values(vectors, 1)?.values_mut()[node] = value;
        Ok(())
    }

    fn matrix_row<E: IdDatum>(
        &mut self,
        vectors: &Vectors<E>,
        row: &SpiHeapTupleData<'_>,
    ) -> Result<(), ComputeSqlError> {
        let Some(node) = self.node(vectors, row)? else {
            return Ok(());
        };
        let Some(elements) = row.get::<Vec<Option<T>>>(2)? else {
            let width = self.values.as_ref().map_or(1, Vector::width);
            let null = Self::null()?;
            self.values(vectors, width)?.values_mut()[node * width..][..width].fill(null);
            return Ok(());
        };
        let width = elements.len();
        if width == 0 {
            return Err(ComputeError::ZeroWidth.into());
        }
        let target = &mut self.values(vectors, width)?.values_mut()[node * width..][..width];
        for (target, element) in target.iter_mut().zip(elements) {
            *target = match element {
                Some(value) => value,
                None => Self::null()?,
            };
        }
        Ok(())
    }

    /// The vector; of width 1 if the query returned no rows.
    fn finish<E: IdDatum>(self, vectors: &Vectors<E>) -> Result<AnyVector, ComputeSqlError> {
        Ok(match self.values {
            Some(values) => values.into(),
            None => Vector::filled(&vectors.memory, vectors.id_space(), 1, self.fill)?.into(),
        })
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::{failure, rows, small};

    fn values(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('t', '{name}', NULL::bigint)"
        ))
    }

    #[pg_test]
    fn query_rows_round_trip_and_missing_nodes_get_fill() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'x',
                 'SELECT id, id * 1.5 FROM t_nodes WHERE id <> 3 UNION ALL SELECT 99, 1
                  UNION ALL SELECT NULL, 1;', fill := 7)",
        )?;
        assert_eq!(values("x")?.as_deref(), Some("1:1.5 2:3 3:7 4:6 5:7.5 6:9"));
        let same = Spi::get_one::<bool>(
            "SELECT bool_and(v.value = n.id * 1.5) FROM warren.vector_values('t', 'x', NULL::bigint) v
             JOIN t_nodes n ON n.id = v.node_id WHERE n.id <> 3",
        )?;
        assert_eq!(same, Some(true));
        Ok(())
    }

    #[pg_test]
    fn values_are_cast_by_postgres_to_the_dtype() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'i', 'SELECT id, 2.5 FROM t_nodes', 'i64');
             SELECT warren.vector_from_query('t', 'b', 'SELECT id, id > 3 FROM t_nodes', 'bool');
             SELECT warren.vector_from_query('t', 'n', 'SELECT id, NULL::float8 FROM t_nodes')",
        )?;
        assert_eq!(values("i")?.as_deref(), Some("1:3 2:3 3:3 4:3 5:3 6:3"));
        assert_eq!(values("b")?.as_deref(), Some("1:0 2:0 3:0 4:1 5:1 6:1"));
        assert_eq!(values("n")?.as_deref(), Some("1:NaN 2:NaN 3:NaN 4:NaN 5:NaN 6:NaN"));
        Ok(())
    }

    #[pg_test]
    fn array_values_make_a_matrix() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'm',
                 'SELECT id, ARRAY[id, -id / 4.0] FROM t_nodes WHERE id < 6', 'f32', fill := 0.5)",
        )?;
        let found = rows(
            "SELECT node_id || ':' || row::text FROM warren.matrix_rows('t', 'm', NULL::bigint)",
        )?;
        assert_eq!(
            found.as_deref(),
            Some("1:{1,-0.25} 2:{2,-0.5} 3:{3,-0.75} 4:{4,-1} 5:{5,-1.25} 6:{0.5,0.5}")
        );
        let ragged = failure(
            "warren.vector_from_query('t', 'r', 'SELECT id, ARRAY[1, 2] FROM t_nodes WHERE id = 1
                                                 UNION ALL SELECT 2, ARRAY[1.0]')",
        )?;
        assert_eq!(
            ragged.as_deref(),
            Some("22023: query rows have 1 values where earlier rows had 2")
        );
        Ok(())
    }

    #[pg_test]
    fn the_query_cannot_write_even_through_a_volatile_function() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE TABLE t_log (n int);
             CREATE FUNCTION t_write() RETURNS bigint VOLATILE LANGUAGE plpgsql AS
                 $f$ BEGIN INSERT INTO t_log VALUES (1); RETURN 1; END $f$",
        )?;
        let refused =
            failure("warren.vector_from_query('t', 'x', 'SELECT id, t_write() FROM t_nodes')")?;
        let refused = refused.unwrap_or_default();
        assert!(
            refused.starts_with("25006: cannot execute INSERT in a read-only transaction"),
            "{refused}"
        );
        assert_eq!(Spi::get_one::<i64>("SELECT count(*) FROM t_log")?, Some(0));
        Ok(())
    }

    #[pg_test]
    fn bad_queries_and_rows_are_errors() -> Result<(), Error> {
        small("t")?;
        let cases = [
            ("'SELECT id FROM t_nodes'", "42804: query must return 2 (id, value) columns, not 1"),
            (
                "'SELECT 1::bigint, 1 UNION ALL SELECT 1, 2'",
                "21000: query returns more than one row for node 1",
            ),
            (
                "'SELECT 1::bigint, NULL::int', 'i64'",
                "22004: null value cannot be stored in a i64 vector",
            ),
            (
                "'DELETE FROM t_nodes RETURNING id, id'",
                "42601: query must be a single SELECT (or VALUES, or TABLE)",
            ),
            ("'SELECT 1::bigint, 1; SELECT 2::bigint, 2'", "42601: query must be a single SELECT"),
            (
                "'SELECT 2.5::float8, 1'",
                "42804: query column 1 (id) is of type double precision, which does not cast \
                 implicitly to the graph's id type bigint",
            ),
            (
                "'SELECT ''1''::text, 1'",
                "42804: query column 1 (id) is of type text, which does not cast implicitly",
            ),
        ];
        for (args, expected) in cases {
            let found = failure(&format!("warren.vector_from_query('t', 'x', {args})"))?;
            let found = found.unwrap_or_default();
            assert!(found.starts_with(expected), "{args}: {found}");
        }
        Ok(())
    }

    #[pg_test]
    fn ids_cast_implicitly_and_tombstoned_ids_are_skipped_even_when_repeated() -> Result<(), Error>
    {
        small("t")?;
        Spi::run("DELETE FROM t_nodes WHERE id = 2")?;
        crate::fixture::refresh("t", true)?;
        Spi::run(
            "SELECT warren.vector_from_query('t', 'x',
                 'SELECT id::int, 5 FROM t_nodes WHERE id = 1
                  UNION ALL SELECT 2, 1 UNION ALL SELECT 2, 2')",
        )?;
        assert_eq!(values("x")?.as_deref(), Some("1:5 3:0 4:0 5:0 6:0"));
        Ok(())
    }

    #[pg_test]
    fn the_query_runs_as_the_caller_under_the_callers_search_path() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "CREATE SCHEMA scores; CREATE TABLE scores.s (id bigint, v float8);
             INSERT INTO scores.s VALUES (1, 10);
             CREATE ROLE analyst; SELECT warren.grant('t', 'analyst');
             GRANT USAGE ON SCHEMA scores TO analyst",
        )?;
        Spi::run(
            "SET search_path = scores, public;
             SELECT warren.vector_from_query('t', 's', 'SELECT id, v FROM s');
             RESET search_path",
        )?;
        assert_eq!(values("s")?.as_deref(), Some("1:10 2:0 3:0 4:0 5:0 6:0"));
        Spi::run("SET ROLE analyst")?;
        let denied = failure("warren.vector_from_query('t', 's', 'SELECT id, v FROM scores.s')")?;
        Spi::run("RESET ROLE")?;
        assert_eq!(denied.as_deref(), Some("42501: permission denied for table s"));
        Ok(())
    }
}
