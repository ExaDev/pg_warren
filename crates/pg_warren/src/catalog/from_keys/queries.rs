//! The node and edge queries a description of the listed tables' keys
//! gives (DESIGN.md §9.3, §14 item 52): SQL text out, nothing read or run.
//!
//! Every name is quoted and schema-qualified, operators included, so the
//! queries mean the same under any `search_path`. An edge joins the row
//! its foreign key references, so its endpoints are that row's own key, as
//! its node has it, whatever the referencing columns' types and whichever
//! unique key the foreign key references; a row whose foreign key is NULL
//! (in any column: MATCH SIMPLE) joins nothing and is no edge.

use std::collections::BTreeSet;

use super::keys::{Description, ForeignKey, KeyColumn, Table};
use super::quote::{ident, literal, readable};
use super::KeysError;
use crate::source::IdType;

/// A graph's source queries, as `warren.create_graph` takes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Queries {
    /// One column: each node's id.
    pub(super) node_query: String,
    /// `src, dst`, then the relation's name when there are several.
    pub(super) edge_query: String,
    /// Whether the edge query has a relation column.
    pub(super) has_relations: bool,
}

/// Between the queries' `SELECT`s.
const UNION: &str = "\nUNION ALL\n";

/// The queries of the graph whose nodes are the rows of `description`'s
/// tables and whose edges are their foreign keys between listed tables,
/// one relation each, and the rows of their junction tables, a relation
/// each ([`Listed::junction_keys`]).
pub(super) fn queries(description: &Description) -> Result<Queries, KeysError> {
    let links = links(description);
    let listed = Listed::all(description, &links)?;
    let node_tables: Vec<&Listed<'_>> = listed.iter().filter(|t| t.junction.is_none()).collect();
    let first = node_tables.first().ok_or(KeysError::NoTables)?;
    let ids = Ids::of(&node_tables);
    let nodes: Vec<String> = node_tables
        .iter()
        .map(|t| format!("SELECT {} FROM {} n", ids.of_row(t, "n"), from(t.table)))
        .collect();
    let relations = relations(&listed, &links, &ids);
    let has_relations = relations.len() > 1;
    let edges: Vec<String> = relations
        .iter()
        .zip(relation_names(&relations))
        .map(|(relation, name)| {
            let name = if has_relations { format!(", {}", literal(&name)) } else { String::new() };
            format!("SELECT {}, {}{name} FROM {}", relation.src, relation.dst, relation.joins)
        })
        .collect();
    let edge_query = if edges.is_empty() {
        let id = ids.of_row(first, "n");
        format!("SELECT {id}, {id} FROM {} n WHERE false", from(first.table))
    } else {
        edges.join(UNION)
    };
    Ok(Queries { node_query: nodes.join(UNION), edge_query, has_relations })
}

/// The foreign keys from a listed table to a listed table, by the
/// referencing table's place in the list, then by name.
fn links(description: &Description) -> Vec<&ForeignKey> {
    let place = |oid| description.tables.iter().position(|t| t.oid == oid);
    let mut links: Vec<(usize, &ForeignKey)> = description
        .foreign_keys
        .iter()
        .filter(|fk| place(fk.to).is_some())
        .filter_map(|fk| Some((place(fk.from)?, fk)))
        .collect();
    links.sort_by(|(a, x), (b, y)| a.cmp(b).then_with(|| x.name.cmp(&y.name)));
    links.into_iter().map(|(_, fk)| fk).collect()
}

/// A listed table, its primary key, and, for a junction table, its two
/// foreign keys in key order.
struct Listed<'a> {
    table: &'a Table,
    key: &'a [KeyColumn],
    junction: Option<[&'a ForeignKey; 2]>,
}

impl<'a> Listed<'a> {
    /// The listed tables, each with its primary key (a table without one
    /// is refused), and which are junctions: those whose two foreign keys
    /// could make them one ([`Self::junction_keys`]), which no listed
    /// table references and neither of whose ends could be one.
    fn all(description: &'a Description, links: &[&'a ForeignKey]) -> Result<Vec<Self>, KeysError> {
        if description.tables.is_empty() {
            return Err(KeysError::NoTables);
        }
        let mut listed = description
            .tables
            .iter()
            .map(|table| match &table.key {
                Some(key) if !key.is_empty() => Ok(Self { table, key, junction: None }),
                _ => Err(KeysError::NoPrimaryKey(qualified(table))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let candidates: Vec<Option<[&ForeignKey; 2]>> =
            listed.iter().map(|t| t.junction_keys(links)).collect();
        let could_be = |oid| {
            listed.iter().zip(&candidates).any(|(t, keys)| t.table.oid == oid && keys.is_some())
        };
        let junctions: Vec<Option<[&ForeignKey; 2]>> = listed
            .iter()
            .zip(&candidates)
            .map(|(t, keys)| {
                keys.filter(|ends| {
                    !links.iter().any(|fk| fk.to == t.table.oid)
                        && ends.iter().all(|fk| !could_be(fk.to))
                })
            })
            .collect();
        for (t, junction) in listed.iter_mut().zip(junctions) {
            t.junction = junction;
        }
        Ok(listed)
    }

    /// The two foreign keys that could make this table a junction, in key
    /// order: it has no column but its primary key, which is exactly the
    /// columns of its two foreign keys to other listed tables, and it has
    /// no other foreign key to a listed table.
    fn junction_keys(&self, links: &[&'a ForeignKey]) -> Option<[&'a ForeignKey; 2]> {
        let own: Vec<&ForeignKey> =
            links.iter().copied().filter(|fk| fk.from == self.table.oid).collect();
        let [a, b] = own.as_slice() else { return None };
        let key: BTreeSet<&str> = self.key.iter().map(|c| c.name.as_str()).collect();
        let columns: Vec<&str> = a.columns.iter().chain(&b.columns).map(String::as_str).collect();
        let covers =
            columns.len() == key.len() && columns.iter().copied().collect::<BTreeSet<_>>() == key;
        let others = a.to != self.table.oid && b.to != self.table.oid;
        if !(covers && others && self.table.columns == key.len()) {
            return None;
        }
        let position = |fk: &ForeignKey| {
            fk.columns.iter().filter_map(|c| self.key.iter().position(|k| &k.name == c)).min()
        };
        Some(if position(a) <= position(b) { [*a, *b] } else { [*b, *a] })
    }
}

/// How node ids are written.
enum Ids {
    /// The key itself: one table of nodes, keyed by one column of an id
    /// type whose equality is bytewise.
    Key,
    /// `schema.table:` and the key as text: a row literal, `(a,b)`, for a
    /// composite key.
    Text,
}

impl Ids {
    fn of(node_tables: &[&Listed<'_>]) -> Self {
        match node_tables {
            [only] => match only.key {
                [column] if column.deterministic && IdType::from_oid(column.type_oid).is_some() => {
                    Self::Key
                }
                _ => Self::Text,
            },
            _ => Self::Text,
        }
    }

    /// The id of `table`'s row `alias`.
    fn of_row(&self, table: &Listed<'_>, alias: &str) -> String {
        let columns: Vec<String> =
            table.key.iter().map(|c| format!("{alias}.{}", ident(&c.name))).collect();
        match (self, columns.as_slice()) {
            (Self::Key, [column]) => column.clone(),
            (_, [column]) => text_id(table.table, column),
            _ => text_id(table.table, &format!("ROW({})", columns.join(", "))),
        }
    }
}

/// `schema.table:` and `value` as text, compared bytewise.
fn text_id(table: &Table, value: &str) -> String {
    let prefix = literal(&format!("{}:", qualified(table)));
    format!("({prefix} OPERATOR(pg_catalog.||) {value}::pg_catalog.text) COLLATE pg_catalog.\"C\"")
}

/// `table` as messages, text ids and relation names show it.
fn qualified(table: &Table) -> String {
    format!("{}.{}", readable(&table.schema), readable(&table.name))
}

/// `table` in a `FROM` clause: `ONLY` unless it is partitioned, since a
/// primary key covers every partition, but no inheritance child.
fn from(table: &Table) -> String {
    let name = format!("{}.{}", ident(&table.schema), ident(&table.name));
    if table.partitioned {
        name
    } else {
        format!("ONLY {name}")
    }
}

/// An edge query's `SELECT` for one relation, but for its name.
struct Relation<'a> {
    /// The table it is declared on, and its name there.
    table: &'a Table,
    name: &'a str,
    /// The source and target ids, and the `FROM` clause they come from.
    src: String,
    dst: String,
    joins: String,
}

/// The relations of the listed tables, in list order: each node table's
/// foreign keys to listed tables (`links`), and each junction.
fn relations<'a>(
    listed: &'a [Listed<'a>],
    links: &[&'a ForeignKey],
    ids: &Ids,
) -> Vec<Relation<'a>> {
    let find = |oid| listed.iter().find(|t| t.table.oid == oid);
    let mut found = Vec::new();
    for t in listed {
        if let Some([a, b]) = t.junction {
            let (Some(src), Some(dst)) = (find(a.to), find(b.to)) else { continue };
            found.push(Relation {
                table: t.table,
                name: &t.table.name,
                src: ids.of_row(src, "s"),
                dst: ids.of_row(dst, "d"),
                joins: format!(
                    "{} j JOIN {} s ON {} JOIN {} d ON {}",
                    from(t.table),
                    from(src.table),
                    on(a, "s", "j"),
                    from(dst.table),
                    on(b, "d", "j")
                ),
            });
            continue;
        }
        for fk in links.iter().filter(|fk| fk.from == t.table.oid) {
            let Some(dst) = find(fk.to) else { continue };
            found.push(Relation {
                table: t.table,
                name: &fk.name,
                src: ids.of_row(t, "s"),
                dst: ids.of_row(dst, "d"),
                joins: format!(
                    "{} s JOIN {} d ON {}",
                    from(t.table),
                    from(dst.table),
                    on(fk, "d", "s")
                ),
            });
        }
    }
    found
}

/// The join condition of `fk`: each referenced column of row
/// `referenced` equal, by the constraint's own operator, to its
/// referencing column of row `referencing`.
fn on(fk: &ForeignKey, referenced: &str, referencing: &str) -> String {
    let pairs: Vec<String> = fk
        .referenced
        .iter()
        .zip(&fk.columns)
        .zip(&fk.equality)
        .map(|((to, by), op)| {
            format!(
                "{referenced}.{} OPERATOR({}.{}) {referencing}.{}",
                ident(to),
                ident(&op.schema),
                op.name,
                ident(by)
            )
        })
        .collect();
    pairs.join(" AND ")
}

/// The relations' names: each its own, or, where two share one, its
/// table's qualified name, a dot and its own.
fn relation_names(relations: &[Relation<'_>]) -> Vec<String> {
    relations
        .iter()
        .map(|r| {
            if relations.iter().filter(|other| other.name == r.name).count() > 1 {
                format!("{}.{}", qualified(r.table), r.name)
            } else {
                r.name.to_owned()
            }
        })
        .collect()
}

#[cfg(test)]
mod unit {
    use pgrx::pg_sys;

    use super::super::keys::{Description, ForeignKey, KeyColumn, Operator, Table};
    use super::{queries, KeysError, Queries};

    fn column(name: &str, type_oid: pg_sys::Oid) -> KeyColumn {
        KeyColumn { name: name.to_owned(), type_oid, deterministic: true }
    }

    /// Table `public.<name>` with oid `oid`, `columns` columns and primary
    /// key `key`.
    fn table(oid: u32, name: &str, columns: usize, key: Vec<KeyColumn>) -> Table {
        Table {
            oid: pg_sys::Oid::from(oid),
            schema: "public".to_owned(),
            name: name.to_owned(),
            partitioned: false,
            columns,
            key: Some(key),
        }
    }

    /// Foreign key `name` from table `from`'s `columns` to table `to`'s
    /// `referenced`, compared with `pg_catalog.=`.
    fn foreign_key(
        name: &str,
        from: u32,
        columns: &[&str],
        to: u32,
        referenced: &[&str],
    ) -> ForeignKey {
        let equals = Operator { schema: "pg_catalog".to_owned(), name: "=".to_owned() };
        ForeignKey {
            name: name.to_owned(),
            from: pg_sys::Oid::from(from),
            to: pg_sys::Oid::from(to),
            columns: columns.iter().map(|&c| c.to_owned()).collect(),
            referenced: referenced.iter().map(|&c| c.to_owned()).collect(),
            equality: vec![equals; columns.len()],
        }
    }

    fn generated(tables: Vec<Table>, foreign_keys: Vec<ForeignKey>) -> Queries {
        queries(&Description { tables, foreign_keys }).expect("queries")
    }

    /// The text id of table `public.<table>`'s row `alias`, keyed by `key`.
    fn text_id(table: &str, alias: &str, key: &str) -> String {
        format!(
            "('public.{table}:' OPERATOR(pg_catalog.||) {alias}.\"{key}\"::pg_catalog.text) \
             COLLATE pg_catalog.\"C\""
        )
    }

    /// The shop: customers, their orders, products, and order_lines, a
    /// junction of orders and products.
    fn shop() -> (Vec<Table>, Vec<ForeignKey>) {
        (
            vec![
                table(1, "customers", 2, vec![column("id", pg_sys::INT8OID)]),
                table(2, "orders", 3, vec![column("id", pg_sys::INT8OID)]),
                table(3, "products", 2, vec![column("sku", pg_sys::TEXTOID)]),
                table(
                    4,
                    "order_lines",
                    2,
                    vec![
                        column("order_id", pg_sys::INT8OID),
                        column("product_sku", pg_sys::TEXTOID),
                    ],
                ),
            ],
            vec![
                foreign_key("orders_customer_id_fkey", 2, &["customer_id"], 1, &["id"]),
                foreign_key("order_lines_product_sku_fkey", 4, &["product_sku"], 3, &["sku"]),
                foreign_key("order_lines_order_id_fkey", 4, &["order_id"], 2, &["id"]),
            ],
        )
    }

    #[test]
    fn one_table_keyed_by_an_id_type_uses_its_key_as_the_id() {
        for id_type in
            [pg_sys::INT4OID, pg_sys::INT8OID, pg_sys::UUIDOID, pg_sys::TEXTOID, pg_sys::VARCHAROID]
        {
            let found = generated(vec![table(1, "people", 2, vec![column("id", id_type)])], vec![]);
            assert_eq!(
                found,
                Queries {
                    node_query: "SELECT n.\"id\" FROM ONLY \"public\".\"people\" n".to_owned(),
                    edge_query:
                        "SELECT n.\"id\", n.\"id\" FROM ONLY \"public\".\"people\" n WHERE false"
                            .to_owned(),
                    has_relations: false,
                }
            );
        }
    }

    #[test]
    fn a_foreign_key_to_its_own_table_joins_the_referenced_row() {
        let found = generated(
            vec![table(1, "staff", 2, vec![column("id", pg_sys::INT4OID)])],
            vec![foreign_key("staff_manager_fkey", 1, &["manager"], 1, &["id"])],
        );
        assert_eq!(
            found.edge_query,
            "SELECT s.\"id\", d.\"id\" FROM ONLY \"public\".\"staff\" s \
             JOIN ONLY \"public\".\"staff\" d ON d.\"id\" OPERATOR(\"pg_catalog\".=) s.\"manager\""
        );
        assert!(!found.has_relations);
    }

    #[test]
    fn one_table_with_another_key_has_text_ids() {
        let small = generated(vec![table(1, "t", 1, vec![column("id", pg_sys::INT2OID)])], vec![]);
        assert_eq!(
            small.node_query,
            format!("SELECT {} FROM ONLY \"public\".\"t\" n", text_id("t", "n", "id"))
        );
        let mut ci = column("id", pg_sys::TEXTOID);
        ci.deterministic = false;
        let found = generated(vec![table(1, "t", 1, vec![ci])], vec![]);
        assert_eq!(found.node_query, small.node_query, "a nondeterministic collation");
        assert_eq!(
            found.edge_query,
            format!(
                "SELECT {0}, {0} FROM ONLY \"public\".\"t\" n WHERE false",
                text_id("t", "n", "id")
            )
        );
    }

    /// A composite key is written as a row literal, `(a,b)`, which quotes
    /// any value holding a comma, a parenthesis, a quote or a space.
    #[test]
    fn a_composite_key_is_a_row_literal() {
        let key = vec![column("order_id", pg_sys::INT8OID), column("seq", pg_sys::INT4OID)];
        let found = generated(vec![table(1, "shipments", 3, key)], vec![]);
        assert_eq!(
            found.node_query,
            "SELECT ('public.shipments:' OPERATOR(pg_catalog.||) \
             ROW(n.\"order_id\", n.\"seq\")::pg_catalog.text) COLLATE pg_catalog.\"C\" \
             FROM ONLY \"public\".\"shipments\" n"
        );
    }

    #[test]
    fn several_tables_have_text_ids_and_a_junction_is_an_edge() {
        let (tables, foreign_keys) = shop();
        let found = generated(tables, foreign_keys);
        let nodes: Vec<String> = ["customers", "orders", "products"]
            .iter()
            .zip(["id", "id", "sku"])
            .map(|(t, key)| {
                format!("SELECT {} FROM ONLY \"public\".\"{t}\" n", text_id(t, "n", key))
            })
            .collect();
        assert_eq!(found.node_query, nodes.join("\nUNION ALL\n"));
        let placed = format!(
            "SELECT {}, {}, 'orders_customer_id_fkey' FROM ONLY \"public\".\"orders\" s \
             JOIN ONLY \"public\".\"customers\" d ON d.\"id\" OPERATOR(\"pg_catalog\".=) s.\"customer_id\"",
            text_id("orders", "s", "id"),
            text_id("customers", "d", "id"),
        );
        let lines = format!(
            "SELECT {}, {}, 'order_lines' FROM ONLY \"public\".\"order_lines\" j \
             JOIN ONLY \"public\".\"orders\" s ON s.\"id\" OPERATOR(\"pg_catalog\".=) j.\"order_id\" \
             JOIN ONLY \"public\".\"products\" d ON d.\"sku\" OPERATOR(\"pg_catalog\".=) j.\"product_sku\"",
            text_id("orders", "s", "id"),
            text_id("products", "d", "sku"),
        );
        assert_eq!(found.edge_query, format!("{placed}\nUNION ALL\n{lines}"));
        assert!(found.has_relations);
    }

    #[test]
    fn a_composite_foreign_key_joins_on_every_column() {
        let shipments = vec![column("order_id", pg_sys::INT8OID), column("seq", pg_sys::INT4OID)];
        let found = generated(
            vec![
                table(1, "shipments", 2, shipments),
                table(2, "parcels", 3, vec![column("id", pg_sys::INT8OID)]),
            ],
            vec![foreign_key("parcels_shipment_fkey", 2, &["ord", "seq"], 1, &["order_id", "seq"])],
        );
        assert!(
            found.edge_query.ends_with(
                "FROM ONLY \"public\".\"parcels\" s JOIN ONLY \"public\".\"shipments\" d \
                 ON d.\"order_id\" OPERATOR(\"pg_catalog\".=) s.\"ord\" \
                 AND d.\"seq\" OPERATOR(\"pg_catalog\".=) s.\"seq\""
            ),
            "{}",
            found.edge_query
        );
        assert!(found.edge_query.contains("ROW(d.\"order_id\", d.\"seq\")::pg_catalog.text"));
        assert!(!found.has_relations, "one relation has no relation column");
    }

    /// A junction's other columns would be lost as an edge, so it stays a
    /// table of nodes; so does one another listed table references.
    #[test]
    fn a_junction_with_more_columns_or_referenced_is_a_table_of_nodes() {
        let (mut tables, foreign_keys) = shop();
        tables[3].columns = 3;
        let found = generated(tables, foreign_keys.clone());
        assert_eq!(found.node_query.matches("UNION ALL").count(), 3, "{}", found.node_query);
        assert_eq!(found.edge_query.matches("UNION ALL").count(), 2, "{}", found.edge_query);
        assert!(found.edge_query.contains("'order_lines_order_id_fkey'"));

        let (mut tables, mut foreign_keys) = shop();
        tables.push(table(5, "returns", 3, vec![column("id", pg_sys::INT8OID)]));
        foreign_keys.push(foreign_key(
            "returns_line_fkey",
            5,
            &["order_id", "sku"],
            4,
            &["order_id", "product_sku"],
        ));
        let found = generated(tables, foreign_keys);
        assert!(found.node_query.contains("\"order_lines\" n"), "{}", found.node_query);
    }

    /// Foreign keys to tables that are not listed are ignored, so a
    /// junction one of whose ends is not listed is a table of nodes.
    #[test]
    fn foreign_keys_to_tables_not_listed_are_ignored() {
        let (mut tables, foreign_keys) = shop();
        tables.remove(2);
        let found = generated(tables, foreign_keys);
        assert!(!found.edge_query.contains("products"), "{}", found.edge_query);
        assert!(found.node_query.contains("\"order_lines\" n"), "{}", found.node_query);
        assert!(found.edge_query.contains("'order_lines_order_id_fkey'"));
    }

    /// A junction of one table's rows leaves that table the only one of
    /// nodes, keyed by its own ids.
    #[test]
    fn a_junction_over_one_table_keeps_its_ids() {
        let found = generated(
            vec![
                table(1, "people", 2, vec![column("id", pg_sys::INT8OID)]),
                table(
                    2,
                    "follows",
                    2,
                    vec![column("src", pg_sys::INT8OID), column("dst", pg_sys::INT8OID)],
                ),
            ],
            vec![
                foreign_key("follows_dst_fkey", 2, &["dst"], 1, &["id"]),
                foreign_key("follows_src_fkey", 2, &["src"], 1, &["id"]),
            ],
        );
        assert_eq!(found.node_query, "SELECT n.\"id\" FROM ONLY \"public\".\"people\" n");
        assert_eq!(
            found.edge_query,
            "SELECT s.\"id\", d.\"id\" FROM ONLY \"public\".\"follows\" j \
             JOIN ONLY \"public\".\"people\" s ON s.\"id\" OPERATOR(\"pg_catalog\".=) j.\"src\" \
             JOIN ONLY \"public\".\"people\" d ON d.\"id\" OPERATOR(\"pg_catalog\".=) j.\"dst\""
        );
        assert!(!found.has_relations);
    }

    #[test]
    fn relation_names_two_tables_share_are_qualified() {
        let found = generated(
            vec![
                table(1, "a", 2, vec![column("id", pg_sys::INT8OID)]),
                table(2, "b", 2, vec![column("id", pg_sys::INT8OID)]),
            ],
            vec![
                foreign_key("link", 1, &["b_id"], 2, &["id"]),
                foreign_key("link", 2, &["a_id"], 1, &["id"]),
            ],
        );
        assert!(found.edge_query.contains(", 'public.a.link' FROM"), "{}", found.edge_query);
        assert!(found.edge_query.contains(", 'public.b.link' FROM"), "{}", found.edge_query);
    }

    #[test]
    fn names_are_quoted_and_partitioned_tables_read_whole() {
        let mut odd = table(1, "O'Neil \"x\"", 2, vec![column("Key", pg_sys::INT8OID)]);
        odd.schema = "My.Schema".to_owned();
        let mut whole = table(2, "parts", 2, vec![column("id", pg_sys::INT8OID)]);
        whole.partitioned = true;
        let found = generated(vec![odd, whole], vec![]);
        assert_eq!(
            found.node_query,
            "SELECT ('\"My.Schema\".\"O''Neil \"\"x\"\"\":' OPERATOR(pg_catalog.||) \
             n.\"Key\"::pg_catalog.text) COLLATE pg_catalog.\"C\" \
             FROM ONLY \"My.Schema\".\"O'Neil \"\"x\"\"\" n\nUNION ALL\n\
             SELECT ('public.parts:' OPERATOR(pg_catalog.||) n.\"id\"::pg_catalog.text) \
             COLLATE pg_catalog.\"C\" FROM \"public\".\"parts\" n"
        );
    }

    #[test]
    fn no_tables_or_a_table_without_a_primary_key_is_refused() {
        assert_eq!(queries(&Description::default()), Err(KeysError::NoTables));
        let mut keyless = table(1, "Log", 2, vec![]);
        keyless.key = None;
        let refused = queries(&Description { tables: vec![keyless], foreign_keys: vec![] });
        assert_eq!(refused, Err(KeysError::NoPrimaryKey("public.\"Log\"".to_owned())));
    }
}
