//! Tests: id arrays read from a table column rather than written as
//! literals. Postgres stores a short array with a one-byte header and a
//! large one compressed or out of line, so the function reads a detoasted
//! copy; every start, node filter and step filter must be read while that
//! copy is alive (found in review of the SQL fuzzer's first fix).

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use crate::fixture::rows;

    /// Elements in the large array: past glibc's largest mmap threshold
    /// (32 MB) for either id type, so a copy freed too early is unmapped
    /// and reading it faults rather than reading stale bytes.
    const LARGE: i64 = 4_500_000;

    /// Graph `name` with ids of `id_type`, `id(i)` for nodes 1..6 in a
    /// chain 1 → 2 → … → 6, and table `{name}_ids (kind, ids)` holding,
    /// as `short`, ids 1 and 2, and as `large`, ids 1 and 2 followed by
    /// [`LARGE`] copies of an id the graph does not have.
    fn stored(name: &str, id_type: &str, id: &str) -> Result<(), Error> {
        let of = |i: &str| id.replace("{i}", i);
        Spi::run(&format!(
            "CREATE TABLE {name}_nodes (id {id_type});
             CREATE TABLE {name}_edges (src {id_type}, dst {id_type});
             INSERT INTO {name}_nodes SELECT {node} FROM generate_series(1, 6) i;
             INSERT INTO {name}_edges SELECT {src}, {dst} FROM generate_series(1, 5) i;
             SELECT warren.create_graph('{name}', 'SELECT id FROM {name}_nodes',
                                        'SELECT src, dst FROM {name}_edges');
             SELECT warren.refresh('{name}');
             CREATE TABLE {name}_ids (kind text, ids {id_type}[]);
             ALTER TABLE {name}_ids ALTER COLUMN ids SET STORAGE EXTENDED;
             INSERT INTO {name}_ids VALUES
                 ('short', ARRAY[{one}, {two}]),
                 ('large', ARRAY[{one}, {two}] || array_fill({unknown}, ARRAY[{LARGE}]))",
            node = of("i"),
            src = of("i"),
            dst = of("(i + 1)"),
            one = of("1"),
            two = of("2"),
            unknown = of("99"),
        ))
    }

    /// `select` (over `ids`) for each kind of stored array, as
    /// `kind=rows` text.
    fn each(name: &str, select: &str) -> Result<Vec<String>, Error> {
        ["short", "large"]
            .iter()
            .map(|kind| {
                let found = rows(&format!(
                    "SELECT r::text FROM {name}_ids, LATERAL ({select}) s(r) WHERE kind = '{kind}'"
                ))?;
                Ok(format!("{kind}={}", found.unwrap_or_default()))
            })
            .collect()
    }

    /// Every function that takes an id array reads `name`'s stored arrays
    /// as it reads literals; `ids` are the external ids of nodes 1, 2, 3.
    fn read_while_alive(name: &str, ids: [&str; 3]) -> Result<(), Error> {
        let [one, two, three] = ids;
        let both = |found: &str| vec![format!("short={found}"), format!("large={found}")];
        let starts = format!(
            "SELECT string_agg(node_id || ':' || depth, ' ' ORDER BY depth, node_id::text)
             FROM warren.bfs('{name}', ids, 1)"
        );
        assert_eq!(each(name, &starts)?, both(&format!("{one}:0 {two}:0 {three}:1")));
        let filtered = format!(
            "SELECT string_agg(node_id::text, ' ' ORDER BY depth)
             FROM warren.bfs('{name}', ARRAY[(ids)[1]], 9, node_filter := ids)"
        );
        assert_eq!(each(name, &filtered)?, both(&format!("{one} {two}")));
        let reachable = format!("SELECT count(*) FROM warren.reachable_set('{name}', ids, 1)");
        assert_eq!(each(name, &reachable)?, both("3"));
        let walked = format!(
            "SELECT string_agg(end_id::text, ' ' ORDER BY end_id::text)
             FROM warren.walk('{name}', ids, ARRAY[warren.s(NULL)])"
        );
        let mut ends = [two, three];
        ends.sort_unstable();
        assert_eq!(each(name, &walked)?, both(&ends.join(" ")));
        let step = "SELECT ((warren.s(NULL, 'out', ids)).filter = ids::text[])::text";
        assert_eq!(each(name, step)?, both("true"));
        Ok(())
    }

    #[pg_test]
    fn text_ids_stored_short_or_large_are_read_while_alive() -> Result<(), Error> {
        stored("st", "text", "('n' || {i})")?;
        read_while_alive("st", ["n1", "n2", "n3"])
    }

    #[pg_test]
    fn uuid_ids_stored_short_or_large_are_read_while_alive() -> Result<(), Error> {
        stored("su", "uuid", "md5(({i})::text)::uuid")?;
        let id = |i: i32| -> Result<String, Error> {
            let id = Spi::get_one::<String>(&format!("SELECT md5('{i}')::uuid::text"))?;
            Ok(id.unwrap_or_default())
        };
        read_while_alive("su", [&id(1)?, &id(2)?, &id(3)?])
    }
}
