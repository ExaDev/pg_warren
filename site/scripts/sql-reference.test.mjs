import assert from 'node:assert/strict';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { tmpdir } from 'node:os';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { parseSqlFunctions, prepareSqlReference, renderSqlReference, workspaceVersion } from './sql-reference.mjs';

const repository = fileURLToPath(new URL('../../', import.meta.url));

test('parses multiline defaults and returns without treating quoted SQL bodies as declarations', () => {
  const functions = parseSqlFunctions(`
    -- CREATE FUNCTION warren.comment() RETURNS void;
    CREATE OR REPLACE FUNCTION "warren"."walk"(
      graph text, options text DEFAULT $value$a,b ) RETURNS text$value$,
      weights int[] DEFAULT ARRAY[1, 2], expression text DEFAULT 'it''s (a, b)'
    ) RETURNS TABLE (node_id bigint, value double precision)
    LANGUAGE plpgsql AS $body$
    BEGIN
      -- CREATE FUNCTION warren.fake() RETURNS void;
      PERFORM 'CREATE FUNCTION warren.fake() RETURNS void';
    END;
    $body$;
    CREATE FUNCTION warren.walk(graph text) RETURNS SETOF bigint LANGUAGE c AS 'MODULE_PATHNAME', 'walk_wrapper';
    CREATE FUNCTION other.ignore() RETURNS void LANGUAGE sql AS $$ SELECT NULL $$;
    CREATE FUNCTION warren.helper() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RETURN NEW; END $$;
    CREATE FUNCTION warren.event() RETURNS event_trigger LANGUAGE plpgsql AS $$ BEGIN END $$;
  `);
  assert.equal(functions.length, 2);
  assert.deepEqual(functions.map((fn) => fn.name), ['walk', 'walk']);
  assert.equal(functions[0].arguments.length, 4);
  assert.match(functions[0].arguments[1], /\$value\$a,b \) RETURNS text\$value\$/);
  assert.equal(functions[0].arguments[2], 'weights int[] DEFAULT ARRAY[1, 2]');
  assert.equal(functions[0].arguments[3], "expression text DEFAULT 'it''s (a, b)'");
  assert.equal(functions[0].returns, 'TABLE (node_id bigint, value double precision)');
  assert.equal(functions[1].returns, 'SETOF bigint');
});

test('removes pgrx type comments while preserving SQL strings and quoted identifiers', () => {
  const [fn] = parseSqlFunctions(`CREATE  FUNCTION warren."grant"(
    "name" TEXT /* Option < & str > */, role regrole DEFAULT NULL
  ) RETURNS void /* Rust */ STRICT VOLATILE LANGUAGE c AS 'MODULE_PATHNAME', 'grant_wrapper';`);
  assert.equal(fn.name, 'grant');
  assert.equal(fn.qualifiedName, 'warren."grant"');
  assert.deepEqual(fn.arguments, ['"name" TEXT', 'role regrole DEFAULT NULL']);
  assert.equal(fn.returns, 'void');
});

test('derives the software version only from workspace.package', () => {
  assert.equal(workspaceVersion('[package]\nversion = "0.1.0"\n[workspace.package]\nversion = "9.8.7"\n[dependencies]\nversion = "3.2.1"'), '9.8.7');
  assert.throws(() => workspaceVersion('[package]\nversion = "1.0.0"'), /workspace.package/);
});

test('renders overloads and fails visibly if new public functions need categorisation', () => {
  const functions = parseSqlFunctions(`CREATE FUNCTION warren.bfs(graph text) RETURNS void LANGUAGE c AS 'x', 'y';
    CREATE FUNCTION warren.bfs(graph text, start bigint) RETURNS bigint LANGUAGE c AS 'x', 'z';`);
  const output = renderSqlReference(functions, '9.8.7');
  assert.match(output, /9\.8\.7.*development/i);
  assert.equal((output.match(/^### `warren.bfs`/gm) ?? []).length, 1);
  assert.equal((output.match(/warren\.bfs\(/g) ?? []).length, 2);
  assert.match(output, /RETURNS bigint;/);
  assert.throws(() => renderSqlReference([{ name: 'new_function', qualifiedName: 'warren.new_function', arguments: [], returns: 'void' }], '9.8.7'), /new_function/);
});

test('current install SQL includes the complete direct-call surface and representative defaults', async () => {
  const toml = await readFile(path.join(repository, 'Cargo.toml'), 'utf8');
  const version = workspaceVersion(toml);
  const sql = await readFile(path.join(repository, `sql/pg_warren--${version}.sql`), 'utf8');
  const functions = parseSqlFunctions(sql);
  const names = new Set(functions.map((fn) => fn.name));
  for (const name of ['create_graph', 'refresh', 'bfs', 'steps', 's', 'spmv', 'vector_values', 'pagerank', 'eigen', 'distance', 'louvain', 'leiden', 'set_vector_quota', 'backends', 'progress', 'report_progress', 'version']) {
    assert.ok(names.has(name), `Missing ${name}`);
  }
  const declarations = [...sql.matchAll(/^CREATE\s+(?:OR\s+REPLACE\s+)?FUNCTION\s+warren\./gm)].length;
  assert.equal(functions.length, declarations - 4, 'only the three catalog triggers and one event trigger are excluded');
  assert.ok(functions.find((fn) => fn.name === 'bfs').arguments.includes('max_depth int'));
  assert.ok(functions.find((fn) => fn.name === 'pagerank').arguments.includes('damping float8 DEFAULT 0.85'));
  assert.equal(functions.find((fn) => fn.name === 'reachable_set').returns, 'SETOF anyelement');
  assert.match(functions.find((fn) => fn.name === 'progress').returns, /^TABLE \(/);
  assert.doesNotThrow(() => renderSqlReference(functions, version));
});

test('generation reads the current workspace version and writes deterministic Markdown', async () => {
  const directory = await mkdtemp(path.join(tmpdir(), 'pg-warren-reference-'));
  try {
    await mkdir(path.join(directory, 'sql'));
    await writeFile(path.join(directory, 'Cargo.toml'), '[workspace.package]\nversion = "9.8.7"\n');
    await writeFile(path.join(directory, 'sql/pg_warren--9.8.7.sql'), 'CREATE FUNCTION warren.version() RETURNS text LANGUAGE sql AS $$ SELECT \'9.8.7\' $$;');
    const destination = path.join(directory, 'generated');
    await prepareSqlReference(directory, destination);
    const filename = path.join(destination, 'docs/reference.md');
    const generated = await readFile(filename, 'utf8');
    assert.match(generated, /^---\ntitle: SQL function reference/);
    assert.match(generated, /pg_warren--9\.8\.7\.sql/);
    assert.match(generated, /warren\.version\(\)/);
    assert.match(generated, /\\df\+ warren\.\*/);
    await prepareSqlReference(directory, destination);
    assert.equal(await readFile(filename, 'utf8'), generated);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
