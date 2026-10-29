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
use std::sync::Arc;

use warren_core::compute::{
    gather as fan_in, AnyVector, Bytes, Combine, Combiner, ComputeError, Dtype, Gather, Mask,
    Messages, RowAt, SemiringElement,
};
use warren_core::graph::RelationSelection;

use super::from_query::{check_id_column, id_type_name};
use super::vectors::Vectors;
use super::{entry, ComputeSqlError};
use crate::access::{with_search_path, Caller};
use crate::cache::{resolve, CachedIds, Resolved};
use crate::extension::added_flag;
use crate::external_ids::check_type;
use crate::interrupts::workers;
use crate::interrupts::PostgresInterrupts;
use crate::read_only::read_one;
use crate::selection::{direction, relations};
use crate::source::{describe, type_name, with_id_type, SourceError};
pub(crate) use codec::TypeError;
use codec::{Codec, ResultType};
use emit::{Build, Row, Stream};
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
    include_self: bool,
    per_relation: bool,
    /// The witnesses' types.
    id_type: pg_sys::Oid,
    value_type: pg_sys::Oid,
}

/// Where the messages are read from, once prepared.
enum Input {
    /// A width-1 vector: its values take the fixed-width path.
    Vector(Arc<AnyVector>),
    /// A query of `(id, value)` columns, wrapped to cast them, read with
    /// `codec`.
    Query {
        /// How its messages are read.
        codec: Codec,
        /// The wrapped query.
        query: String,
    },
}

impl Input {
    /// How the messages are read.
    fn codec(&self, combiner: Combiner) -> Result<Codec, ComputeSqlError> {
        Ok(match self {
            Self::Vector(vector) => Codec::choose(dtype_type(vector.dtype()), combiner)?,
            Self::Query { codec, .. } => *codec,
        })
    }
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

/// The qualified SQL name of a type, without a type modifier: `bpchar`
/// for `char`, whose bare name `character` means `character(1)`.
fn qualified(oid: pg_sys::Oid) -> String {
    const FLAGS: u32 = pg_sys::FORMAT_TYPE_FORCE_QUALIFY | pg_sys::FORMAT_TYPE_TYPEMOD_GIVEN;
    // SAFETY: format_type_extended returns a palloc'd NUL-terminated string
    // for any type oid; typmod -1 is no modifier.
    unsafe { std::ffi::CStr::from_ptr(pg_sys::format_type_extended(oid, -1, FLAGS as u16)) }
        .to_string_lossy()
        .into_owned()
}

/// Whether `name` is a relation on the caller's `search_path`.
fn relation_visible(caller: &Caller, name: &str) -> bool {
    with_search_path(caller.search_path(), || {
        read_one::<pg_sys::Oid>("SELECT pg_catalog.to_regclass($1)", &[name.into()])
    })
    .ok()
    .flatten()
    .is_some()
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
    /// Where the messages come from.
    fn prepare<E: CachedIds>(
        &self,
        vectors: &mut Vectors<E>,
        caller: &Caller,
    ) -> Result<Input, ComputeSqlError> {
        let names = vectors.names()?;
        let is_vector = |name: &str| names.iter().any(|bound| bound.name == name);
        let is_relation = |name: &str| relation_visible(caller, name);
        let query = match MessageSource::parse(self.messages, is_vector, is_relation) {
            MessageSource::Ambiguous(name) => {
                return Err(ComputeSqlError::AmbiguousMessages {
                    name: name.to_owned(),
                    version: vectors.version,
                })
            }
            MessageSource::Vector(name) => {
                let vector = vectors.get(name)?;
                if vector.width() != 1 {
                    return Err(ComputeSqlError::InvalidArgument(format!(
                        "vector \"{name}\" has width {}; gather messages from a query of its rows",
                        vector.width()
                    )));
                }
                return Ok(Input::Vector(vector));
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
        Ok(Input::Query { codec, query: wrapped })
    }

    /// Gathers at id type `E`.
    fn run<E: CachedIds>(
        &self,
        resolved: Resolved,
        caller: &Caller,
    ) -> Result<Rows, ComputeSqlError> {
        check_type::<E>(self.graph, "id_type", self.id_type, false)?;
        let mut vectors = Vectors::<E>::open(resolved)?;
        let input = self.prepare(&mut vectors, caller)?;
        let codec = input.codec(self.combiner)?;
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
        let run = Run {
            arguments: self,
            vectors: &vectors,
            caller,
            mask: mask.as_deref(),
            result,
            codec,
        };
        match (input, codec) {
            (Input::Vector(vector), _) => match vector.dtype() {
                Dtype::F64 => run.vector::<f64>(&vector),
                Dtype::F32 => run.vector::<f32>(&vector),
                Dtype::I64 => run.vector::<i64>(&vector),
                Dtype::Bool => run.vector::<bool>(&vector),
            },
            (Input::Query { query, .. }, Codec::Element { dtype, .. }) => match dtype {
                Dtype::F64 => run.elements::<f64>(&query),
                Dtype::F32 => run.elements::<f32>(&query),
                Dtype::I64 => run.elements::<i64>(&query),
                Dtype::Bool => run.elements::<bool>(&query),
            },
            (Input::Query { query, .. }, Codec::Payload { .. }) => run.payloads(&query),
            (Input::Query { query, .. }, Codec::Binary { sql }) => run.binary(&query, sql),
        }
    }
}

/// The result rows, streamed.
type Rows = Box<dyn Iterator<Item = Row>>;

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

impl<E: CachedIds> Run<'_, E> {
    /// Fans `messages` out, combines them, and streams the result, whose
    /// single values `build` makes.
    fn stream<M: Combine + 'static>(
        &self,
        messages: Messages<M>,
        build: Build<M>,
    ) -> Result<Rows, ComputeSqlError> {
        let loaded = &self.vectors.loaded;
        let selected = relations(&loaded.relations, self.arguments.rels.as_deref());
        let spec = Gather {
            combiner: self.arguments.combiner,
            direction: self.arguments.direction,
            relations: selected.as_deref().map_or(RelationSelection::All, RelationSelection::Only),
            mask: self.mask.map(|vector| Mask { vector, complement: self.arguments.complement }),
            include_self: self.arguments.include_self,
            per_relation: self.arguments.per_relation,
        };
        let memory = &self.vectors.memory;
        let gathered =
            fan_in(&*workers()?, memory, &mut PostgresInterrupts, &loaded.graph, &spec, &messages)?;
        drop(messages);
        Ok(Box::new(Stream {
            loaded: Arc::clone(loaded),
            gathered,
            result: self.result,
            message: self.codec.message_type(),
            build,
            at: RowAt::default(),
        }))
    }

    /// Every live node's value of width-1 `vector`, of element type `T`.
    fn vector<T: SqlElement>(&self, vector: &AnyVector) -> Result<Rows, ComputeSqlError> {
        let values = T::vector(vector)
            .ok_or(ComputeError::DtypeMismatch { expected: T::DTYPE, found: vector.dtype() })?
            .values();
        let mut messages = Messages::<T>::new(&self.vectors.memory)?;
        read::from_vector(self.vectors, values, &mut messages)?;
        self.stream(messages, Box::new(|value: T, sql| value.datum(sql)))
    }

    /// `query`'s fixed-width messages, read as element type `T`.
    fn elements<T: SqlElement>(&self, query: &str) -> Result<Rows, ComputeSqlError> {
        let mut messages = Messages::<T>::new(&self.vectors.memory)?;
        read::from_query(self.vectors, self.caller, query, |node, row| {
            read::element(&mut messages, node, row)
        })?;
        self.stream(messages, Box::new(|value: T, sql| value.datum(sql)))
    }

    /// `query`'s `text` or `bytea` payloads, for `concat`.
    fn payloads(&self, query: &str) -> Result<Rows, ComputeSqlError> {
        let mut messages = Messages::<Bytes>::new(&self.vectors.memory)?;
        read::from_query(self.vectors, self.caller, query, |node, row| {
            read::payload(&mut messages, node, row)
        })?;
        self.stream(messages, Box::new(|bytes: &[u8], _| emit::payload(bytes)))
    }

    /// `query`'s messages of type `sql`, in their binary send form.
    fn binary(&self, query: &str, sql: pg_sys::Oid) -> Result<Rows, ComputeSqlError> {
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
        self.stream(messages, emit::receiver(receive, ioparam))
    }
}

/// `include_self`'s position: 0.4.0's definition does not pass it.
const INCLUDE_SELF: usize = 10;
/// `per_relation`'s position: 0.6.0's definition does not pass it.
const PER_RELATION: usize = 11;

/// `warren.gather`: each node that receives a message, and its messages
/// combined. See the module documentation for `id_type` and `value_type`.
/// `include_self` and `per_relation`, added since releases, are read from
/// `fcinfo` ([`added_flag`]).
#[pg_extern(
    schema = "warren",
    sql = r#"
CREATE FUNCTION warren.gather(graph text, messages text, id_type anyelement,
                              value_type anycompatible, combine text DEFAULT 'collect',
                              direction text DEFAULT 'in', rels text[] DEFAULT NULL,
                              mask text DEFAULT NULL, mask_complement boolean DEFAULT false,
                              version bigint DEFAULT NULL, include_self boolean DEFAULT false,
                              per_relation boolean DEFAULT false)
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
            include_self: added_flag(fcinfo, INCLUDE_SELF)
                .ok_or(ComputeSqlError::NullArgument("include_self"))?,
            per_relation: added_flag(fcinfo, PER_RELATION)
                .ok_or(ComputeSqlError::NullArgument("per_relation"))?,
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
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests;
