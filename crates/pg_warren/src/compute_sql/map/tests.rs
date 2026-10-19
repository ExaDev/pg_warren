//! In-database tests of `vector_map` and `explain_map`.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;
    use pgrx::spi::Error;

    use super::super::{force_executor, last_was_jit_compiled};
    use crate::fixture::{failure, refresh, ring, rows, small};

    fn path(graph: &str, expr: &str) -> Result<Option<String>, Error> {
        let quoted = expr.replace('\'', "''");
        Spi::get_one::<String>(&format!(
            "SELECT warren.explain_map('{graph}', '{quoted}') ->> 'path'"
        ))
    }

    fn values(name: &str) -> Result<Option<String>, Error> {
        rows(&format!(
            "SELECT node_id || ':' || value FROM warren.vector_values('t', '{name}', NULL::bigint)"
        ))
    }

    /// The vectors Appendix A's programs map over, on graph `t`.
    fn appendix_vectors() -> Result<(), Error> {
        small("t")?;
        Spi::run(
            "SELECT warren.vector_degree('t', 'deg');
             SELECT warren.vector_const('t', 'pr', 1);
             SELECT warren.vector_const('t', 'pr_next', 0.5);
             SELECT warren.vector_const('t', 'next', 0.5);
             SELECT warren.vector_const('t', 'gathered', 0.25);
             SELECT warren.vector_from_query('t', 'seed', 'SELECT id, 1.0 FROM t_nodes WHERE id < 3');
             SELECT warren.vector_from_query('t', 'is_account', 'SELECT id, 1.0 FROM t_nodes WHERE id % 2 = 0');
             SELECT warren.vector_const('t', 'score', 0.1);
             SELECT warren.vector_const('t', 'inv_deg', 0.5);
             SELECT warren.vector_const('t', 'live', 1, 'i64');
             SELECT warren.vector_from_query('t', 'emb', 'SELECT id, ARRAY[id, 2 * id] FROM t_nodes', 'f32');
             SELECT warren.vector_from_query('t', 'emb_nbr', 'SELECT id, ARRAY[1, 1] FROM t_nodes', 'f32')",
        )
    }

    #[pg_test]
    fn appendix_a_expressions_take_the_compiled_path() -> Result<(), Error> {
        appendix_vectors()?;
        let expressions = [
            "0.15 + 0.85 * pr_next / NULLIF(deg, 0)",
            "CASE WHEN is_account = 1 OR deg <= 50 THEN 1 ELSE 0 END",
            "CASE WHEN deg > 0 THEN 1.0 / deg ELSE 0 END",
            "seed",
            "score * inv_deg * live",
            "0.30000000000000004 * seed + 0.7 * gathered",
            "next",
            "pr / NULLIF(deg, 0)",
            "0.15000000000000002 + 0.85 * gathered",
            "0.7 * emb + 0.3 * emb_nbr",
        ];
        for expr in expressions {
            assert_eq!(path("t", expr)?.as_deref(), Some("compiled"), "{expr}");
        }
        Ok(())
    }

    #[pg_test]
    fn a_map_writes_the_expressions_values_and_type() -> Result<(), Error> {
        appendix_vectors()?;
        Spi::run(
            "SELECT warren.vector_map('t', 'msg', 'pr / NULLIF(deg, 0)');
             SELECT warren.vector_map('t', 'flag', 'CASE WHEN deg > 1 THEN 1 ELSE 0 END');
             SELECT warren.vector_map('t', 'smooth', '0.5 * emb + emb_nbr')",
        )?;
        assert_eq!(values("msg")?.as_deref(), Some("1:0.5 2:1 3:1 4:1 5:1 6:NaN"));
        assert_eq!(values("flag")?.as_deref(), Some("1:1 2:0 3:0 4:0 5:0 6:0"));
        let dtypes = rows(
            "SELECT name || ':' || dtype || ':' || width FROM warren.vectors
             WHERE name IN ('msg', 'flag', 'smooth') ORDER BY name",
        )?;
        assert_eq!(dtypes.as_deref(), Some("flag:i64:1 msg:f64:1 smooth:f64:2"));
        let smooth = rows(
            "SELECT row::text FROM warren.matrix_rows('t', 'smooth', NULL::bigint) WHERE node_id = 2",
        )?;
        assert_eq!(smooth.as_deref(), Some("{2,3}"));
        Ok(())
    }

    #[pg_test]
    fn sql_outside_the_compiled_subset_falls_back_to_the_executor() -> Result<(), Error> {
        appendix_vectors()?;
        Spi::run(
            "CREATE FUNCTION public.twice(x float8) RETURNS float8
                 LANGUAGE sql IMMUTABLE AS 'SELECT 2 * x'",
        )?;
        let reason = Spi::get_one::<String>(
            "SELECT warren.explain_map('t', 'pr::numeric + 1') ->> 'reason'",
        )?;
        assert_eq!(reason.as_deref(), Some("function numeric is not in the compiled subset"));
        for expr in
            ["twice(deg)", "pr::numeric + 1", "deg::int + 1", "CASE WHEN deg IS NULL THEN 1 END"]
        {
            assert_eq!(path("t", expr)?.as_deref(), Some("executor"), "{expr}");
        }
        Spi::run(
            "SET search_path = public, pg_catalog;
             SELECT warren.vector_map('t', 'doubled', 'twice(deg) + (deg::int + 1)');
             RESET search_path",
        )?;
        assert_eq!(values("doubled")?.as_deref(), Some("1:7 2:4 3:4 4:4 5:4 6:1"));
        Ok(())
    }

    #[pg_test]
    fn the_executor_evaluates_under_the_callers_search_path() -> Result<(), Error> {
        appendix_vectors()?;
        // The function's body names `params` unqualified: it resolves when
        // the body is first run, so only under the caller's search_path.
        Spi::run(
            "CREATE TABLE public.params (w float8); INSERT INTO public.params VALUES (3);
             CREATE FUNCTION public.weighted(x float8) RETURNS float8
                 LANGUAGE sql STABLE AS 'SELECT x * w FROM params'",
        )?;
        assert_eq!(path("t", "public.weighted(deg)")?.as_deref(), Some("executor"));
        Spi::run(
            "SET search_path = public, pg_catalog;
             SELECT warren.vector_map('t', 'w', 'weighted(deg)');
             RESET search_path",
        )?;
        assert_eq!(values("w")?.as_deref(), Some("1:6 2:3 3:3 4:3 5:3 6:0"));
        Ok(())
    }

    #[pg_test]
    fn expressions_that_are_not_one_scalar_expression_are_refused() -> Result<(), Error> {
        appendix_vectors()?;
        let cases = [
            ("pr FROM pg_class", "42601: a vector_map expression must be one SQL expression"),
            ("1; SELECT 2", "42601: a vector_map expression must be one SQL expression"),
            ("sum(pr)", "0A000: a vector_map expression cannot contain aggregates"),
            ("(SELECT 1.0)", "0A000: a vector_map expression cannot contain aggregates"),
            ("'text'::text", "42804: a vector_map expression must be of type double precision"),
            ("nope + 1", "42703: column \"nope\" does not exist"),
        ];
        for (expr, expected) in cases {
            let quoted = expr.replace('\'', "''");
            let found = failure(&format!("warren.vector_map('t', 'y', '{quoted}')"))?;
            let found = found.unwrap_or_default();
            assert!(found.starts_with(expected), "{expr}: {found}");
        }
        Ok(())
    }

    /// The outcome of mapping `expr` into `y`: its error, or its stored
    /// dtype and bytes.
    fn outcome(graph: &str, y: &str, expr: &str) -> Result<String, Error> {
        let quoted = expr.replace('\'', "''");
        if let Some(error) = failure(&format!("warren.vector_map('{graph}', '{y}', '{quoted}')"))? {
            return Ok(error);
        }
        Ok(Spi::get_one::<String>(&format!(
            "SELECT v.dtype || ':' || v.width || ':'
                    || string_agg(encode(c.data, 'hex'), '' ORDER BY c.chunk_no)
             FROM warren.vectors v JOIN warren.vector_chunks c USING (vector_id)
             JOIN warren.graphs g USING (graph_id)
             WHERE g.name = '{graph}' AND v.name = '{y}' GROUP BY v.dtype, v.width"
        ))?
        .unwrap_or_default())
    }

    /// Both paths' outcomes for `expr`, compiled first.
    fn both(graph: &str, expr: &str) -> Result<(String, String), Error> {
        force_executor(false);
        let compiled = outcome(graph, "yc", expr)?;
        force_executor(true);
        let executed = outcome(graph, "ye", expr);
        force_executor(false);
        Ok((compiled, executed?))
    }

    #[pg_test]
    fn errors_agree_in_sqlstate_and_message() -> Result<(), Error> {
        appendix_vectors()?;
        Spi::run("SELECT warren.vector_const('t', 'zero', 0); SELECT warren.vector_const('t', 'big', 1e308)")?;
        let cases = [
            ("1 / zero", "22012: division by zero"),
            ("big * 10", "22003: value out of range: overflow"),
            ("ln(zero)", "2201E: cannot take logarithm of zero"),
            ("sqrt(zero - 1)", "2201F: cannot take square root of a negative number"),
            ("live * 9223372036854775807 + live", "22003: bigint out of range"),
            ("(big * 0)::real + 1e39::float8::real", "22003: value out of range: overflow"),
            (
                "CASE WHEN deg = 0 THEN NULL ELSE 1 END",
                "22004: null value cannot be stored in a i64 vector",
            ),
        ];
        for (expr, expected) in cases {
            assert_eq!(path("t", expr)?.as_deref(), Some("compiled"), "{expr}");
            let (compiled, executed) = both("t", expr)?;
            assert_eq!(compiled, expected, "{expr}");
            assert_eq!(executed, expected, "{expr}");
        }
        Ok(())
    }

    /// A deterministic pseudo-random sequence (xorshift64*).
    struct Random(u64);

    impl Random {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545_f491_4f6c_dd1d) >> 33) as usize % n
        }

        fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
            items[self.below(items.len())]
        }

        /// A `double precision` expression.
        fn float(&mut self, depth: u32) -> String {
            let leaves = [
                "a",
                "b",
                "e::float8",
                "c::float8",
                "(2.5::float8)",
                "(0.1 * a)",
                "(-3::float8)",
                "(1e308::float8)",
                "(1e-310 + b)",
                "('NaN'::float8)",
                "(0::float8)",
            ];
            if depth == 0 || self.below(4) == 0 {
                return self.pick(&leaves).to_owned();
            }
            let d = depth - 1;
            match self.below(12) {
                0 => format!(
                    "({} {} {})",
                    self.float(d),
                    self.pick(&["+", "-", "*", "/"]),
                    self.float(d)
                ),
                1 => format!("(-{})", self.float(d)),
                2 => format!(
                    "{}({})",
                    self.pick(&[
                        "abs", "sqrt", "exp", "ln", "log", "floor", "ceil", "round", "sign"
                    ]),
                    self.float(d)
                ),
                3 => format!("power({}, {})", self.float(d), self.float(d)),
                4 => format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    self.boolean(d),
                    self.float(d),
                    self.float(d)
                ),
                5 => format!("NULLIF({}, {})", self.float(d), self.float(d)),
                6 => format!(
                    "COALESCE(NULLIF({}, {}), {})",
                    self.float(d),
                    self.float(d),
                    self.float(d)
                ),
                7 => format!(
                    "{}({}, {})",
                    self.pick(&["LEAST", "GREATEST"]),
                    self.float(d),
                    self.float(d)
                ),
                8 => format!("({} + {})", self.float(d), self.int(d)),
                9 => format!("({})::real::float8", self.float(d)),
                10 => {
                    format!("CASE {} WHEN {} THEN {} END", self.int(d), self.int(d), self.float(d))
                }
                _ => format!("({} ^ {})", self.float(d), self.float(d)),
            }
        }

        /// A `bigint` expression.
        fn int(&mut self, depth: u32) -> String {
            let leaves = ["c", "(7::bigint)", "(0::bigint)", "(-9223372036854775807::bigint)"];
            if depth == 0 || self.below(3) == 0 {
                return self.pick(&leaves).to_owned();
            }
            let d = depth - 1;
            match self.below(6) {
                0 => format!(
                    "({} {} {})",
                    self.int(d),
                    self.pick(&["+", "-", "*", "/", "%"]),
                    self.int(d)
                ),
                1 => format!("abs({})", self.int(d)),
                2 => format!("({})::bigint", self.float(d)),
                3 => format!(
                    "CASE WHEN {} THEN {} ELSE {} END",
                    self.boolean(d),
                    self.int(d),
                    self.int(d)
                ),
                4 => format!("GREATEST({}, {})", self.int(d), self.int(d)),
                _ => format!("(-{})", self.int(d)),
            }
        }

        /// A `boolean` expression.
        fn boolean(&mut self, depth: u32) -> String {
            if depth == 0 || self.below(4) == 0 {
                return self
                    .pick(&["d", "(a > b)", "(c = 0)", "(e < 1)", "NULL::boolean"])
                    .to_owned();
            }
            let d = depth - 1;
            let cmp = ["=", "<>", "<", "<=", ">", ">="];
            match self.below(5) {
                0 => format!("({} {} {})", self.float(d), self.pick(&cmp), self.float(d)),
                1 => format!("({} {} {})", self.int(d), self.pick(&cmp), self.int(d)),
                2 => format!(
                    "({} {} {})",
                    self.boolean(d),
                    self.pick(&["AND", "OR"]),
                    self.boolean(d)
                ),
                3 => format!("(NOT {})", self.boolean(d)),
                _ => format!("({} {} {})", self.boolean(d), self.pick(&cmp), self.boolean(d)),
            }
        }
    }

    /// Graph `p`: a 300-node ring with random vectors `a`, `b` (f64), `c`
    /// (i64), `d` (bool) and `e` (f32), with zeros, NaN, infinities and
    /// extremes among them.
    fn random_vectors() -> Result<(), Error> {
        ring("p", 300, 2)?;
        refresh("p", true)?;
        Spi::run(
            "SELECT setseed(0.17);
             CREATE TABLE p_values AS SELECT id,
                 CASE id % 23 WHEN 0 THEN 'NaN' WHEN 1 THEN 'Infinity' WHEN 2 THEN 0
                              WHEN 3 THEN '-0' WHEN 4 THEN 1e300 ELSE (random() - 0.5) * 20 END::float8 AS a,
                 CASE id % 19 WHEN 0 THEN '-Infinity' WHEN 1 THEN 0 ELSE random() * 4 - 1 END::float8 AS b,
                 CASE id % 17 WHEN 0 THEN 0 WHEN 1 THEN 9223372036854775807
                              ELSE floor(random() * 21 - 10)::bigint END AS c,
                 random() < 0.5 AS d,
                 (random() * 3 - 1.5)::real AS e
             FROM p_nodes;
             SELECT warren.vector_from_query('p', 'a', 'SELECT id, a FROM p_values');
             SELECT warren.vector_from_query('p', 'b', 'SELECT id, b FROM p_values');
             SELECT warren.vector_from_query('p', 'c', 'SELECT id, c FROM p_values', 'i64');
             SELECT warren.vector_from_query('p', 'd', 'SELECT id, d FROM p_values', 'bool');
             SELECT warren.vector_from_query('p', 'e', 'SELECT id, e FROM p_values', 'f32')",
        )
    }

    #[pg_test]
    fn compiled_and_executor_paths_agree_bit_for_bit_on_random_expressions() -> Result<(), Error> {
        random_vectors()?;
        let mut random = Random(0x9e37_79b9_7f4a_7c15);
        let (mut values, mut errors) = (0, 0);
        for _ in 0..300 {
            let expr = match random.below(3) {
                0 => random.float(4),
                1 => random.int(3),
                _ => random.boolean(3),
            };
            assert_eq!(path("p", &expr)?.as_deref(), Some("compiled"), "{expr}");
            let (compiled, executed) = both("p", &expr)?;
            assert_eq!(compiled, executed, "{expr}");
            match compiled.contains(": ") {
                true => errors += 1,
                false => values += 1,
            }
        }
        assert!(values > 100 && errors > 10, "{values} values, {errors} errors");
        Ok(())
    }

    #[pg_test]
    fn the_executor_jit_compiles_when_the_jit_settings_say_so() -> Result<(), Error> {
        random_vectors()?;
        let expr = "CASE WHEN d THEN a * b ELSE sqrt(abs(e)) + c END";
        Spi::run("SET LOCAL jit = on; SET LOCAL jit_above_cost = 0")?;
        // Whether this server can load its JIT provider (llvmjit): with one,
        // the executor must JIT-compile; without one (e.g. PG 18 without
        // postgresql-18-jit), it must still evaluate, uncompiled.
        let available = Spi::get_one::<bool>("SELECT pg_jit_available()")?.unwrap_or(false);
        let (compiled, executed) = both("p", expr)?;
        assert_eq!(
            last_was_jit_compiled(),
            available,
            "JIT-compiled must match pg_jit_available() = {available}"
        );
        assert_eq!(compiled, executed);
        Spi::run("SET LOCAL jit_above_cost = 1e9")?;
        let (_, again) = both("p", expr)?;
        assert!(!last_was_jit_compiled());
        assert_eq!(again, executed);
        Ok(())
    }
}
