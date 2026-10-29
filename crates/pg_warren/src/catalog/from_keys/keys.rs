//! What the catalog says about the listed tables' keys: each table's
//! primary key and the foreign keys it declares, as `pg_constraint`,
//! `pg_index` and `pg_attribute` describe them. The generator reads only
//! this, so it is tested against descriptions written by hand.

use pgrx::pg_sys;

/// A listed table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Table {
    /// Its oid, which foreign keys name it by.
    pub(super) oid: pg_sys::Oid,
    /// Its schema's name, unquoted.
    pub(super) schema: String,
    /// Its name, unquoted.
    pub(super) name: String,
    /// Whether it is a partitioned table, whose primary key covers its
    /// partitions (any other table is read `ONLY`).
    pub(super) partitioned: bool,
    /// How many columns it has (dropped ones aside).
    pub(super) columns: usize,
    /// Its primary key's columns, in key order, or `None` without one.
    pub(super) key: Option<Vec<KeyColumn>>,
}

/// A column of a primary key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct KeyColumn {
    /// Its name, unquoted.
    pub(super) name: String,
    /// Its type.
    pub(super) type_oid: pg_sys::Oid,
    /// Whether its collation, if it has one, is deterministic.
    pub(super) deterministic: bool,
}

/// A foreign key a listed table declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ForeignKey {
    /// The constraint's name.
    pub(super) name: String,
    /// The referencing table.
    pub(super) from: pg_sys::Oid,
    /// The referenced table.
    pub(super) to: pg_sys::Oid,
    /// The referencing columns, unquoted, in the constraint's order.
    pub(super) columns: Vec<String>,
    /// The referenced columns, unquoted, matching `columns`.
    pub(super) referenced: Vec<String>,
    /// The equality operator of each column pair (referenced = referencing).
    pub(super) equality: Vec<Operator>,
}

/// A schema-qualified operator.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Operator {
    /// Its schema's name, unquoted.
    pub(super) schema: String,
    /// Its name.
    pub(super) name: String,
}

/// What the catalog says about the listed tables, in list order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct Description {
    /// The listed tables, each once, in the order first listed.
    pub(super) tables: Vec<Table>,
    /// Every foreign key the listed tables declare, whatever it references.
    pub(super) foreign_keys: Vec<ForeignKey>,
}
