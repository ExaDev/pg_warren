//! `warren.gather` (DESIGN.md §6.4): fan per-node messages from a query,
//! a relation or a vector out over the graph, combine them at each
//! receiver in-engine, and return one row per receiver.
//!
//! The result's types come from two witnesses, as `components` takes one:
//! `id_type` (the graph's id type) and `value_type` (the combiner's result
//! type for the messages' type, [`codec`]); both are checked.

mod codec;
mod emit;
mod read;
mod source;

use pgrx::prelude::*;
use pgrx::{AnyElement, FromDatum, IntoDatum};
use warren_core::compute::{
    gather as fan_in, AnyVector, Bytes, Combine, Combiner, Dtype, Gather, Mask, Messages,
    SemiringElement,
};
use warren_core::graph::RelationSelection;

use super::from_query::{check_id_column, id_type_name};
use super::settings::workers;
use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::access::{with_search_path, Caller};
use crate::cache::{resolve, Resolved};
use crate::external_ids::check_type;
use crate::interrupts::PostgresInterrupts;
use crate::selection::{direction, relations};
use crate::source::{describe, type_name, with_id_type, IdDatum, SourceError};
pub(crate) use codec::TypeError;
use codec::{Codec, ResultType};
use emit::Row;
use source::MessageSource;

/// The arguments of a `gather` call.
struct Arguments<'a> {
    graph: &'a str,
    messages: &'a str,
    combiner: Combiner,
    direction: warren_core::graph::Direction,
    rels: Option<Vec<Option<String>>>,
    mask: Option<&'a str>,
    complement: bool,
    /// The witnesses' types.
    id_type: pg_sys::Oid,
    value_type: pg_sys::Oid,
}

/// Where the messages are read from, once prepared.
enum Input {
    /// A width-1 vector.
    Vector(std::sync::Arc<AnyVector>),
    /// A query of `(id, value)` columns, wrapped to cast them.
    Query(String),
}

/// The SQL type of a vector's elements.
fn dtype_type(dtype: Dtype) -> pg_sys::Oid {
    match dtype {
        Dtype::F32 => pg_sys::FLOAT4OID,
        Dtype::F64 => pg_sys::FLOAT8OID,
        Dtype::I64 => pg_sys::INT8OID,
        Dtype::Bool => pg_sys::BOOLOID,
    }
}

/// The base type of a message column (`text` for an untyped literal).
fn base_type(oid: pg_sys::Oid) -> pg_sys::Oid {
    match oid {
        pg_sys::UNKNOWNOID => pg_sys::TEXTOID,
        // SAFETY: getBaseType reads the type cache.
        oid => unsafe { pg_sys::getBaseType(oid) },
    }
}

/// The qualified SQL name of a type.
fn qualified(oid: pg_sys::Oid) -> String {
    // SAFETY: format_type_be_qualified returns a palloc'd NUL-terminated
    // string for any type oid.
    unsafe { std::ffi::CStr::from_ptr(pg_sys::format_type_be_qualified(oid)) }
        .to_string_lossy()
        .into_owned()
}

/// The oid of a result type.
fn result_oid(result: ResultType) -> Result<pg_sys::Oid, ComputeSqlError> {
    match result {
        ResultType::Of(oid) => Ok(oid),
        ResultType::ArrayOf(element) => {
            // SAFETY: both read the type cache.
            let (is_array, array) = unsafe {
                (
                    pg_sys::get_element_type(element) != pg_sys::Oid::INVALID,
                    pg_sys::get_array_type(element),
                )
            };
            match (is_array, array) {
                (true, _) => Ok(element),
                (false, array) if array != pg_sys::Oid::INVALID => Ok(array),
                _ => Err(ComputeSqlError::InvalidArgument(format!(
                    "type {} has no array type to collect into",
                    type_name(element)
                ))),
            }
        }
    }
}

impl Arguments<'_> {
    /// The codec and input of the messages.
    fn prepare<E: IdDatum>(
        &self,
        vectors: &mut Vectors<E>,
        caller: &Caller,
    ) -> Result<(Codec, Input), ComputeSqlError> {
        let names = vectors.names()?;
        let query = match MessageSource::parse(self.messages, |name| {
            names.iter().any(|bound| bound.name == name)
        }) {
            MessageSource::Vector(name) => {
                let vector = vectors.get(name)?;
                if vector.width() != 1 {
                    return Err(ComputeSqlError::InvalidArgument(format!(
                        "vector \"{name}\" has width {}; gather messages from a query of its rows",
                        vector.width()
                    )));
                }
                let codec = Codec::choose(dtype_type(vector.dtype()), self.combiner)?;
                return Ok((codec, Input::Vector(vector)));
            }
            MessageSource::Relation(name) => format!("TABLE {name}"),
            MessageSource::Query(query) => query.trim_end().trim_end_matches(';').to_owned(),
        };
        let columns = with_search_path(caller.search_path(), || {
            Spi::connect(|_| describe::<ComputeSqlError>("messages", &query))
        })?;
        if columns.len() != 2 {
            return Err(SourceError::ColumnCount {
                query: "messages",
                expected: "2 (id, message)",
                found: columns.len(),
            }
            .into());
        }
        check_id_column::<E>(&columns[0])?;
        let codec = Codec::choose(base_type(columns[1].oid), self.combiner)?;
        let wrapped = format!(
            "SELECT q.id::{}, q.value::{} FROM ({query}\n) AS q(id, value)",
            id_type_name::<E>(),
            qualified(codec.read_type())
        );
        Ok((codec, Input::Query(wrapped)))
    }

    /// Gathers at id type `E`.
    fn run<E: IdDatum>(
        &self,
        resolved: Resolved,
        caller: &Caller,
    ) -> Result<Vec<Row>, ComputeSqlError> {
        check_type::<E>(self.graph, "id_type", self.id_type, false)?;
        let mut vectors = Vectors::<E>::open(resolved)?;
        let (codec, input) = self.prepare(&mut vectors, caller)?;
        let result = result_oid(codec.result(self.combiner))?;
        if self.value_type != result {
            return Err(ComputeSqlError::ValueType {
                combiner: self.combiner,
                message: type_name(codec.message_type()),
                result: type_name(result),
                found: type_name(self.value_type),
            });
        }
        let mask = self.mask.map(|name| vectors.get(name)).transpose()?;
        let mut run = Run {
            arguments: self,
            vectors: &vectors,
            caller,
            mask: mask.as_deref(),
            result,
            codec,
        };
        match codec {
            Codec::Element { dtype: Dtype::F64, .. } => run.elements::<f64>(&input),
            Codec::Element { dtype: Dtype::F32, .. } => run.elements::<f32>(&input),
            Codec::Element { dtype: Dtype::I64, .. } => run.elements::<i64>(&input),
            Codec::Element { dtype: Dtype::Bool, .. } => run.elements::<bool>(&input),
            Codec::Payload { sql } => run.bytes(&input, sql == pg_sys::BYTEAOID),
            Codec::Binary { sql } => run.binary(&input, sql),
        }
    }
}

/// A prepared gather.
struct Run<'r, E> {
    arguments: &'r Arguments<'r>,
    vectors: &'r Vectors<E>,
    caller: &'r Caller,
    mask: Option<&'r AnyVector>,
    result: pg_sys::Oid,
    codec: Codec,
}

/// A vector element that is also a SQL value.
trait SqlElement: SemiringElement + FromDatum + IntoDatum {
    /// The element as a datum of SQL type `sql`.
    fn datum(self, sql: pg_sys::Oid) -> Option<pg_sys::Datum>;
}

impl SqlElement for i64 {
    fn datum(self, sql: pg_sys::Oid) -> Option<pg_sys::Datum> {
        emit::integer(self, sql)
    }
}

macro_rules! sql_element {
    ($($type:ty),*) => {$(
        impl SqlElement for $type {
            fn datum(self, _: pg_sys::Oid) -> Option<pg_sys::Datum> {
                self.into_datum()
            }
        }
    )*};
}
sql_element!(f64, f32, bool);

impl<E: IdDatum> Run<'_, E> {
    /// Fans `messages` out and combines them.
    fn gather<M: Combine>(
        &self,
        messages: &Messages<M>,
    ) -> Result<warren_core::compute::Gathered<M>, ComputeSqlError> {
        let loaded = &self.vectors.loaded;
        let selected = relations(&loaded.relations, self.arguments.rels.as_deref());
        let spec = Gather {
            combiner: self.arguments.combiner,
            direction: self.arguments.direction,
            relations: selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only),
            mask: self.mask.map(|vector| Mask { vector, complement: self.arguments.complement }),
        };
        let memory = &self.vectors.memory;
        Ok(fan_in(&*workers()?, memory, &mut PostgresInterrupts, &loaded.graph, &spec, messages)?)
    }

    /// Fixed-width messages of element type `T`.
    fn elements<T: SqlElement>(&mut self, input: &Input) -> Result<Vec<Row>, ComputeSqlError> {
        let mut messages = Messages::<T>::new(&self.vectors.memory)?;
        match input {
            Input::Vector(vector) => {
                let values = T::vector(vector).map(|vector| vector.values()).unwrap_or_default();
                read::from_vector(self.vectors, values, &mut messages)?;
            }
            Input::Query(query) => {
                read::from_query(self.vectors, self.caller, query, |node, row| {
                    read::element(&mut messages, node, row)
                })?
            }
        }
        let gathered = self.gather(&messages)?;
        drop(messages);
        let message = self.codec.message_type();
        Ok(emit::rows(
            &self.vectors.loaded.id_map,
            &gathered,
            self.result,
            message,
            |value: T, sql| value.datum(sql),
        ))
    }

    /// `text` (or, if `bytea`, `bytea`) payloads, for `concat`.
    fn bytes(&mut self, input: &Input, bytea: bool) -> Result<Vec<Row>, ComputeSqlError> {
        let Input::Query(query) = input else { return Ok(Vec::new()) };
        let mut messages = Messages::<Bytes>::new(&self.vectors.memory)?;
        read::from_query(self.vectors, self.caller, query, |node, row| match bytea {
            true => read::bytea(&mut messages, node, row),
            false => read::text(&mut messages, node, row),
        })?;
        let gathered = self.gather(&messages)?;
        drop(messages);
        let message = self.codec.message_type();
        Ok(emit::rows(&self.vectors.loaded.id_map, &gathered, self.result, message, emit::payload))
    }

    /// Messages of type `sql` in their binary send form.
    fn binary(&mut self, input: &Input, sql: pg_sys::Oid) -> Result<Vec<Row>, ComputeSqlError> {
        let Input::Query(query) = input else { return Ok(Vec::new()) };
        let (mut send, mut varlena, mut receive, mut ioparam) =
            (pg_sys::Oid::INVALID, false, pg_sys::Oid::INVALID, pg_sys::Oid::INVALID);
        // SAFETY: the out-pointers point to live locals; both raise an
        // ERROR for a type without binary I/O.
        unsafe {
            pg_sys::getTypeBinaryOutputInfo(sql, &mut send, &mut varlena);
            pg_sys::getTypeBinaryInputInfo(sql, &mut receive, &mut ioparam);
        }
        let mut messages = Messages::<Bytes>::new(&self.vectors.memory)?;
        read::from_query(self.vectors, self.caller, query, |node, row| {
            read::binary(send, &mut messages, node, row)
        })?;
        let gathered = self.gather(&messages)?;
        drop(messages);
        let rebuild = |bytes: &[u8], _| emit::received(bytes, receive, ioparam);
        Ok(emit::rows(&self.vectors.loaded.id_map, &gathered, self.result, sql, rebuild))
    }
}

/// `warren.gather`: each node that receives a message, and its messages
/// combined. See the module documentation for `id_type` and `value_type`.
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.gather(graph text, messages text, id_type anyelement,
                              value_type anycompatible, combine text DEFAULT 'collect',
                              direction text DEFAULT 'in', rels text[] DEFAULT NULL,
                              mask text DEFAULT NULL, mask_complement boolean DEFAULT false,
                              version bigint DEFAULT NULL)
RETURNS TABLE (node_id anyelement, value anycompatible)
VOLATILE PARALLEL UNSAFE LANGUAGE c AS 'MODULE_PATHNAME', 'gather_wrapper';
"#
)]
#[allow(clippy::too_many_arguments)] // The SQL signature's arguments.
fn gather(
    graph: Option<&str>,
    messages: Option<&str>,
    _id_type: Option<AnyElement>,
    _value_type: Option<AnyElement>,
    combine: Option<&str>,
    direction: Option<&str>,
    rels: Option<Vec<Option<String>>>,
    mask: Option<&str>,
    mask_complement: Option<bool>,
    version: Option<i64>,
    fcinfo: pg_sys::FunctionCallInfo,
) -> TableIterator<'static, (name!(node_id, AnyElement), name!(value, AnyElement))> {
    entry(|caller| {
        let graph = graph.ok_or(ComputeSqlError::NullArgument("graph"))?;
        // SAFETY: fcinfo is this call's, and arguments 2 and 3 exist.
        let (id_type, value_type) = unsafe {
            (pgrx::fcinfo::pg_getarg_type(fcinfo, 2), pgrx::fcinfo::pg_getarg_type(fcinfo, 3))
        };
        let arguments = Arguments {
            graph,
            messages: messages.ok_or(ComputeSqlError::NullArgument("messages"))?,
            combiner: combine.ok_or(ComputeSqlError::NullArgument("combine"))?.parse()?,
            direction: self::direction(direction)?,
            rels,
            mask,
            complement: mask_complement.ok_or(ComputeSqlError::NullArgument("mask_complement"))?,
            id_type,
            value_type,
        };
        let resolved = resolve(graph, version, "read")?;
        let rows =
            with_id_type!(resolved.graph.id_type, E => arguments.run::<E>(resolved, caller))?;
        Ok(TableIterator::new(rows))
    })
}

#[cfg(any(test, feature = "pg_test"))]
mod tests;
