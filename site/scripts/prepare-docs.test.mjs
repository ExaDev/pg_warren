import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { prepareDocs, renderTraversal } from './prepare-docs.mjs';

const repository = fileURLToPath(new URL('../../', import.meta.url));

test('renders the canonical traversal page with development metadata and one title', () => {
  const result = renderTraversal('# Traversal\n\nA traversal.\n');
  assert.match(result, /^---\ntitle: Traversal\n/);
  assert.match(result, /description: /);
  assert.match(result, /editUrl: https:\/\/github.com\/ExaDev\/pg_warren\/edit\/main\/docs\/traversal.md/);
  assert.match(result, /text: Development/);
  assert.doesNotMatch(result, /^# Traversal$/m);
  assert.match(result, /A traversal\.\n$/);
});

test('rewrites relative Markdown links while preserving fragments and external links', () => {
  const result = renderTraversal('# Traversal\n\n[Ids](semantics.md#id-types-and-type-witnesses) [Compute](./compute.md) [Design](../DESIGN.md#scope) [External](https://example.com/guide.md) [Here](#paths)\n');
  assert.match(result, /\[Ids\]\(https:\/\/github.com\/ExaDev\/pg_warren\/blob\/main\/docs\/semantics.md#id-types-and-type-witnesses\)/);
  assert.match(result, /\[Compute\]\(https:\/\/github.com\/ExaDev\/pg_warren\/blob\/main\/docs\/compute.md\)/);
  assert.match(result, /\[Design\]\(https:\/\/github.com\/ExaDev\/pg_warren\/blob\/main\/DESIGN.md#scope\)/);
  assert.match(result, /\[External\]\(https:\/\/example.com\/guide.md\)/);
  assert.match(result, /\[Here\]\(#paths\)/);
});

test('preserves fenced SQL and inline code byte for byte', () => {
  const sql = '```sql\nSELECT 1::bigint; -- [example](semantics.md)\n\nSELECT $$a\\nb$$;\n```';
  const tilde = '~~~~sql\nSELECT \'[link](compute.md)\';\n~~~~';
  const inline = '`[example](semantics.md)`';
  const result = renderTraversal(`# Traversal\n\n${sql}\n\n${tilde}\n\n${inline}\n`);
  assert.ok(result.includes(sql));
  assert.ok(result.includes(tilde));
  assert.ok(result.includes(inline));
});

test('imports the actual Markdown without changing its tested SQL blocks', async () => {
  const destination = await mkdtemp(path.join(tmpdir(), 'pg-warren-docs-'));
  try {
    await prepareDocs(repository, destination);
    const source = await readFile(path.join(repository, 'docs/traversal.md'), 'utf8');
    const generated = await readFile(path.join(destination, 'docs/traversal.md'), 'utf8');
    const sqlBlocks = (text) => [...text.matchAll(/^```sql\n[\s\S]*?^```/gm)].map(([block]) => block);
    assert.ok(sqlBlocks(source).length > 0);
    assert.deepEqual(sqlBlocks(generated), sqlBlocks(source));
    assert.ok(generated.includes('test/sql/docs_examples.sql'));
    assert.doesNotMatch(generated, /\]\((?:\.\/)?(?:semantics|compute|limits)\.md/);
    await prepareDocs(repository, destination);
    assert.equal(await readFile(path.join(destination, 'docs/traversal.md'), 'utf8'), generated);
  } finally {
    await rm(destination, { recursive: true, force: true });
  }
});
