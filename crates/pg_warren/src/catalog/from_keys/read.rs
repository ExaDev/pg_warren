//! Reading the listed tables' keys from `pg_class`, `pg_constraint` and
//! `pg_attribute`. These catalogs are readable by every role, so the read
//! runs as the caller; it assigns no transaction id, and runs in parallel
//! mode.

use pgrx::prelude::*;
use pgrx::spi::SpiClient;

use super::keys::{Description, ForeignKey, KeyColumn, Operator, Table};
use super::KeysError;
use crate::catalog::CatalogError;
use crate::read_only::select;

/// Each distinct table of `$1`, in the order first listed, with its
/// schema, name, whether it is partitioned, its columns and whether it has
/// a primary key; schema and name are NULL for an oid of no relation.
const TABLES: &str = "
    SELECT l.relid, n.nspname::pg_catalog.text, c.relname::pg_catalog.text, c.relkind = 'p',
           (SELECT pg_catalog.count(*) FROM pg_catalog.pg_attribute a
            WHERE a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped),
           k.oid IS NOT NULL
    FROM (SELECT DISTINCT ON (u.relid) u.relid, u.position
          FROM pg_catalog.unnest($1::pg_catalog.oid[]) WITH ORDINALITY AS u (relid, position)
          ORDER BY u.relid, u.position) l
    LEFT JOIN pg_catalog.pg_class c ON c.oid = l.relid
    LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
    LEFT JOIN pg_catalog.pg_constraint k ON k.conrelid = c.oid AND k.contype = 'p'
    ORDER BY l.position";

/// The primary key columns of the tables `$1`, in key order: the table,
/// the column's name and type, and whether its collation, if any, is
/// deterministic.
const KEYS: &str = "
    SELECT k.conrelid, a.attname::pg_catalog.text, a.atttypid,
           COALESCE(co.collisdeterministic, true)
    FROM pg_catalog.pg_constraint k
    CROSS JOIN LATERAL pg_catalog.unnest(k.conkey) WITH ORDINALITY AS u (attnum, position)
    JOIN pg_catalog.pg_attribute a ON a.attrelid = k.conrelid AND a.attnum = u.attnum
    LEFT JOIN pg_catalog.pg_collation co ON co.oid = a.attcollation
    WHERE k.contype = 'p' AND k.conrelid = ANY ($1::pg_catalog.oid[])
    ORDER BY k.conrelid, u.position";

/// The foreign keys the tables `$1` declare: name, referencing and
/// referenced table, their columns in the constraint's order, and the
/// schema and name of each pair's equality operator (referenced =
/// referencing, `conpfeqop`).
const FOREIGN_KEYS: &str = "
    SELECT f.conname::pg_catalog.text, f.conrelid, f.confrelid,
           ARRAY(SELECT a.attname::pg_catalog.text
                 FROM pg_catalog.unnest(f.conkey) WITH ORDINALITY AS u (attnum, position)
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = f.conrelid AND a.attnum = u.attnum
                 ORDER BY u.position),
           ARRAY(SELECT a.attname::pg_catalog.text
                 FROM pg_catalog.unnest(f.confkey) WITH ORDINALITY AS u (attnum, position)
                 JOIN pg_catalog.pg_attribute a ON a.attrelid = f.confrelid AND a.attnum = u.attnum
                 ORDER BY u.position),
           ARRAY(SELECT n.nspname::pg_catalog.text
                 FROM pg_catalog.unnest(f.conpfeqop) WITH ORDINALITY AS u (op, position)
                 JOIN pg_catalog.pg_operator o ON o.oid = u.op
                 JOIN pg_catalog.pg_namespace n ON n.oid = o.oprnamespace
                 ORDER BY u.position),
           ARRAY(SELECT o.oprname::pg_catalog.text
                 FROM pg_catalog.unnest(f.conpfeqop) WITH ORDINALITY AS u (op, position)
                 JOIN pg_catalog.pg_operator o ON o.oid = u.op
                 ORDER BY u.position)
    FROM pg_catalog.pg_constraint f
    WHERE f.contype = 'f' AND f.conrelid = ANY ($1::pg_catalog.oid[])";

/// What the catalog says about the tables `listed`: refuses an empty list,
/// a NULL and an oid of no relation.
pub(super) fn describe(listed: &[Option<pg_sys::Oid>]) -> Result<Description, CatalogError> {
    if listed.is_empty() {
        return Err(KeysError::NoTables.into());
    }
    let oids =
        listed.iter().map(|oid| oid.ok_or(KeysError::NullTable)).collect::<Result<Vec<_>, _>>()?;
    Spi::connect(|client| {
        let mut tables = read_tables(client, oids)?;
        let distinct: Vec<pg_sys::Oid> = tables.iter().map(|t| t.oid).collect();
        read_keys(client, &distinct, &mut tables)?;
        let foreign_keys = read_foreign_keys(client, distinct)?;
        Ok(Description { tables, foreign_keys })
    })
}

fn read_tables(client: &SpiClient<'_>, oids: Vec<pg_sys::Oid>) -> Result<Vec<Table>, CatalogError> {
    select(client, TABLES, None, &[oids.into()], |rows| {
        rows.map(|row| {
            let oid = row.get::<pg_sys::Oid>(1)?.unwrap_or(pg_sys::Oid::INVALID);
            let (Some(schema), Some(name)) = (row.get::<String>(2)?, row.get::<String>(3)?) else {
                return Err(KeysError::UndefinedTable(oid.to_u32()).into());
            };
            Ok(Table {
                oid,
                schema,
                name,
                partitioned: row.get::<bool>(4)?.unwrap_or(false),
                columns: usize::try_from(row.get::<i64>(5)?.unwrap_or(0)).unwrap_or(0),
                key: row.get::<bool>(6)?.unwrap_or(false).then(Vec::new),
            })
        })
        .collect()
    })
}

/// Adds each table's primary key columns to its `key`.
fn read_keys(
    client: &SpiClient<'_>,
    oids: &[pg_sys::Oid],
    tables: &mut [Table],
) -> Result<(), CatalogError> {
    select(client, KEYS, None, &[oids.to_vec().into()], |rows| {
        for row in rows {
            let oid = row.get::<pg_sys::Oid>(1)?;
            let column = KeyColumn {
                name: row.get::<String>(2)?.unwrap_or_default(),
                type_oid: row.get::<pg_sys::Oid>(3)?.unwrap_or(pg_sys::Oid::INVALID),
                deterministic: row.get::<bool>(4)?.unwrap_or(true),
            };
            let key = tables.iter_mut().find(|t| Some(t.oid) == oid).and_then(|t| t.key.as_mut());
            key.into_iter().for_each(|key| key.push(column.clone()));
        }
        Ok(())
    })
}

fn read_foreign_keys(
    client: &SpiClient<'_>,
    oids: Vec<pg_sys::Oid>,
) -> Result<Vec<ForeignKey>, CatalogError> {
    select(client, FOREIGN_KEYS, None, &[oids.into()], |rows| {
        rows.map(|row| {
            let schemas = row.get::<Vec<String>>(6)?.unwrap_or_default();
            let names = row.get::<Vec<String>>(7)?.unwrap_or_default();
            Ok(ForeignKey {
                name: row.get::<String>(1)?.unwrap_or_default(),
                from: row.get::<pg_sys::Oid>(2)?.unwrap_or(pg_sys::Oid::INVALID),
                to: row.get::<pg_sys::Oid>(3)?.unwrap_or(pg_sys::Oid::INVALID),
                columns: row.get::<Vec<String>>(4)?.unwrap_or_default(),
                referenced: row.get::<Vec<String>>(5)?.unwrap_or_default(),
                equality: schemas
                    .into_iter()
                    .zip(names)
                    .map(|(schema, name)| Operator { schema, name })
                    .collect(),
            })
        })
        .collect()
    })
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
#[allow(clippy::disallowed_methods)] // Test code may assign a transaction id.
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::keys::{Description, ForeignKey, KeyColumn, Operator};
    use super::describe;
    use crate::catalog::CatalogError;

    /// The oids of the tables `names`, as `describe` takes them.
    fn oids(names: &[&str]) -> Result<Vec<Option<pg_sys::Oid>>, Error> {
        names
            .iter()
            .map(|name| Spi::get_one::<pg_sys::Oid>(&format!("SELECT '{name}'::regclass::oid")))
            .collect()
    }

    fn described(names: &[&str]) -> Result<Description, Error> {
        describe(&oids(names)?).map_err(|_| Error::InvalidPosition)
    }

    fn shop() -> Result<(), Error> {
        Spi::run(
            "CREATE SCHEMA fk;
             CREATE TABLE fk.customers (id bigint PRIMARY KEY, gone int, name text);
             ALTER TABLE fk.customers DROP COLUMN gone;
             CREATE TABLE fk.regions (id int PRIMARY KEY);
             CREATE TABLE fk.orders (id int PRIMARY KEY,
                                     customer_id bigint REFERENCES fk.customers,
                                     region_id int REFERENCES fk.regions);
             CREATE TABLE fk.lines (order_id int REFERENCES fk.orders,
                                    sku text COLLATE \"C\",
                                    PRIMARY KEY (sku, order_id));
             CREATE TABLE fk.log (at timestamptz)",
        )
    }

    #[pg_test]
    fn tables_are_described_once_in_the_order_first_listed() -> Result<(), Error> {
        shop()?;
        let found = described(&["fk.lines", "fk.customers", "fk.lines", "fk.orders", "fk.log"])?;
        let names: Vec<(&str, &str, usize)> =
            found.tables.iter().map(|t| (t.schema.as_str(), t.name.as_str(), t.columns)).collect();
        assert_eq!(
            names,
            [("fk", "lines", 2), ("fk", "customers", 2), ("fk", "orders", 3), ("fk", "log", 1)]
        );
        let key = |i: usize| found.tables[i].key.clone();
        let column = |name: &str, type_oid| KeyColumn {
            name: name.to_owned(),
            type_oid,
            deterministic: true,
        };
        assert_eq!(
            key(0),
            Some(vec![column("sku", pg_sys::TEXTOID), column("order_id", pg_sys::INT4OID)])
        );
        assert_eq!(key(1), Some(vec![column("id", pg_sys::INT8OID)]));
        assert_eq!(key(3), None);
        assert!(found.tables.iter().all(|t| !t.partitioned));
        Ok(())
    }

    #[pg_test]
    fn foreign_keys_are_described_whatever_they_reference() -> Result<(), Error> {
        shop()?;
        let found = described(&["fk.orders", "fk.lines"])?;
        let mut keys: Vec<ForeignKey> = found.foreign_keys;
        keys.sort_by(|a, b| a.name.cmp(&b.name));
        let equals = Operator { schema: "pg_catalog".to_owned(), name: "=".to_owned() };
        let orders = Spi::get_one::<pg_sys::Oid>("SELECT 'fk.orders'::regclass::oid")?;
        let lines = Spi::get_one::<pg_sys::Oid>("SELECT 'fk.lines'::regclass::oid")?;
        let names: Vec<&str> = keys.iter().map(|k| k.name.as_str()).collect();
        assert_eq!(
            names,
            ["lines_order_id_fkey", "orders_customer_id_fkey", "orders_region_id_fkey"]
        );
        assert_eq!(
            keys[0],
            ForeignKey {
                name: "lines_order_id_fkey".to_owned(),
                from: lines.unwrap_or(pg_sys::Oid::INVALID),
                to: orders.unwrap_or(pg_sys::Oid::INVALID),
                columns: vec!["order_id".to_owned()],
                referenced: vec!["id".to_owned()],
                equality: vec![equals],
            }
        );
        assert_eq!(keys[1].equality[0].name, "=", "bigint = bigint");
        Ok(())
    }

    #[pg_test]
    fn collations_and_partitioning_are_described() -> Result<(), Error> {
        Spi::run(
            "CREATE COLLATION fk_ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false);
             CREATE TABLE fk_ci (id text COLLATE fk_ci PRIMARY KEY);
             CREATE TABLE fk_parts (id bigint PRIMARY KEY) PARTITION BY RANGE (id)",
        )?;
        let found = described(&["fk_ci", "fk_parts"])?;
        assert_eq!(found.tables[0].key.as_ref().map(|k| k[0].deterministic), Some(false));
        assert_eq!(found.tables[1].key.as_ref().map(|k| k[0].deterministic), Some(true));
        assert!(found.tables[1].partitioned && !found.tables[0].partitioned);
        Ok(())
    }

    #[pg_test]
    fn an_empty_list_a_null_or_an_unknown_oid_is_refused() -> Result<(), Error> {
        let refused = |listed: &[Option<pg_sys::Oid>]| match describe(listed) {
            Err(CatalogError::Keys(error)) => Some(error.to_string()),
            _ => None,
        };
        assert_eq!(refused(&[]).as_deref(), Some("tables must list at least one table"));
        assert_eq!(refused(&[None]).as_deref(), Some("tables must not hold NULL"));
        assert_eq!(
            refused(&[Some(pg_sys::Oid::from(4_000_000_000))]).as_deref(),
            Some("relation with OID 4000000000 does not exist")
        );
        Ok(())
    }
}
