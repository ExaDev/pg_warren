import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { fileURLToPath } from 'node:url';
import { generatedPages, extractSection, renderPage, routeForSource, prepareDocs } from './prepare-docs.mjs';
import { workspaceVersion } from './sql-reference.mjs';
import { examplePages, expandExampleSql } from './examples.mjs';

const repository = fileURLToPath(new URL('../../', import.meta.url));
const traversal = { slug: 'traversal', title: 'Traversal', source: 'docs/traversal.md' };
const sqlBlocks = (text) => [...text.matchAll(/^```sql\n[\s\S]*?^```/gm)].map(([block]) => block);

test('covers all canonical guides, README sections, changelog, benchmarks, and contributing', () => {
  assert.deepEqual(generatedPages.map(({ slug }) => slug), [
    'semantics', 'traversal', 'compute', 'communities', 'change-detection', 'limits',
    'installation', 'quickstart', 'performance', 'positioning', 'changelog', 'benchmarks', 'contributing',
    ...examplePages.map(({ slug }) => slug),
  ]);
});

test('renders source metadata, development notice, and original title anchor without duplicate h1', () => {
  const result = renderPage('# Traversal\n\nA traversal.\n', traversal);
  assert.match(result, /^---\ntitle: "Traversal"\n/);
  assert.match(result, /editUrl: "https:\/\/github.com\/ExaDev\/pg_warren\/edit\/main\/docs\/traversal.md"/);
  assert.doesNotMatch(result, /sidebar:\n/);
  assert.match(result, /Development documentation/);
  assert.match(result, /id="traversal"/);
  assert.doesNotMatch(result, /^# Traversal$/m);
  assert.match(result, /A traversal\.\n$/);
});

test('resolves cross-page links, README section aliases, original anchors, and unmapped sources', () => {
  assert.equal(routeForSource('docs/semantics.md', 'id-types-and-type-witnesses'), '/pg_warren/docs/semantics/#id-types-and-type-witnesses');
  assert.equal(routeForSource('README.md', 'install'), '/pg_warren/docs/installation/#install');
  assert.equal(routeForSource('README.md', 'how-it-compares'), '/pg_warren/docs/performance/#how-it-compares');
  assert.equal(routeForSource('README.md', 'documentation'), '/pg_warren/docs/');
  assert.equal(routeForSource('docs/positioning.md', 'pggraph-and-pg_warren'), '/pg_warren/docs/positioning/#pggraph-and-pg_warren');
  assert.equal(routeForSource('README.md', 'the-sql-surface-at-a-glance'), 'https://github.com/ExaDev/pg_warren/blob/main/README.md#the-sql-surface-at-a-glance');
  const result = renderPage('# Traversal\n\n[Ids](semantics.md#id-types-and-type-witnesses) [Compute](./compute.md) [Design](../DESIGN.md#scope) [External](https://example.com/guide.md) [Here](#paths) [File](../test/sql/docs_examples.sql)\n', traversal);
  assert.ok(result.includes('[Ids](/pg_warren/docs/semantics/#id-types-and-type-witnesses)'));
  assert.ok(result.includes('[Compute](/pg_warren/docs/compute/)'));
  assert.ok(result.includes('[Design](https://github.com/ExaDev/pg_warren/blob/main/DESIGN.md#scope)'));
  assert.ok(result.includes('[File](https://github.com/ExaDev/pg_warren/blob/main/test/sql/docs_examples.sql)'));
  assert.ok(result.includes('[External](https://example.com/guide.md)'));
  assert.ok(result.includes('[Here](#paths)'));
});

test('extracts exactly a README section, including nested headings and SQL', () => {
  const source = '# README\n\n## Before\nno\n## Install\nkeep\n### Nested\n```sql\nSELECT 1;\n```\n## After\nno\n';
  assert.equal(extractSection(source, 'Install'), '## Install\nkeep\n### Nested\n```sql\nSELECT 1;\n```\n');
  assert.throws(() => extractSection(source, 'Missing'), /Missing/);
});

test('preserves all fenced SQL and inline code bytes while adapting only prose links', () => {
  const sql = '```sql\nSELECT 1::bigint; -- [example](semantics.md)\n\nSELECT $$a\\nb$$;\n```';
  const tilde = '~~~~sql\nSELECT \'[link](compute.md)\';\n~~~~';
  const inline = '`[example](semantics.md)`';
  const result = renderPage(`# Traversal\n\n${sql}\n\n${tilde}\n\n${inline}\n`, traversal);
  assert.ok(result.includes(sql));
  assert.ok(result.includes(tilde));
  assert.ok(result.includes(inline));
});

test('installation never treats a recorded release as proof that packages are live', () => {
  const installation = generatedPages.find(({ slug }) => slug === 'installation');
  const markdown = '## Install\n\nCanonical installation instructions.\n';
  const developing = renderPage(markdown, installation, { current: '9.2.0', latest: '9.1.0' });
  assert.match(developing, /Package availability/);
  assert.ok(developing.includes('**9.2.0**'));
  assert.ok(developing.includes('**9.1.0**'));
  const recorded = renderPage(markdown, installation, { current: '9.2.0', latest: '9.2.0' });
  assert.match(recorded, /Package availability/);
  assert.match(recorded, /does not establish/);
  assert.match(recorded, /apt, Docker, or Homebrew/);
  assert.match(recorded, /published release notes and assets/);
  assert.match(recorded, /Canonical installation instructions/);
});

test('all imported pages retain canonical SQL, resolved links, and deterministic output', async () => {
  const destination = await mkdtemp(path.join(tmpdir(), 'pg-warren-docs-'));
  try {
    const manifest = await readFile(path.join(repository, 'Cargo.toml'), 'utf8');
    const recorded = await readFile(path.join(repository, 'sql/released.commits'), 'utf8');
    const current = workspaceVersion(manifest);
    const latest = recorded.trim().split(/\s+/).at(-1);
    await prepareDocs(repository, destination, { reference: false, downloads: false });
    const outputs = new Map();
    for (const page of generatedPages) {
      const raw = await readFile(path.join(repository, page.source), 'utf8');
      const source = await expandExampleSql(page.section ? extractSection(raw, page.section) : raw, repository);
      const filename = path.join(destination, 'docs', `${page.slug}.md`);
      const generated = await readFile(filename, 'utf8');
      assert.deepEqual(sqlBlocks(generated), sqlBlocks(source), page.slug);
      assert.doesNotMatch(generated, /\]\((?:\.\/)?(?:semantics|compute|limits)\.md/);
      assert.match(generated, /Development documentation/);
      if (page.slug === 'installation') {
        assert.ok(generated.includes(':::caution[Package availability]'));
        if (current !== latest) {
          assert.ok(generated.includes(`**${current}**`));
          assert.ok(generated.includes(`**${latest}**`));
        }
        const fingerprint = source.match(/`([^`\n]+)` <!-- apt key -->/);
        assert.ok(fingerprint, 'canonical installation documents its apt signing fingerprint');
        assert.ok(generated.includes(fingerprint[0]), 'the current canonical signing fingerprint remains unchanged');
      }
      if (page.slug === 'quickstart') assert.ok(generated.includes('[installing](/pg_warren/docs/installation/#install)'));
      outputs.set(filename, generated);
    }
    await prepareDocs(repository, destination, { reference: false, downloads: false });
    for (const [filename, generated] of outputs) {
      assert.equal(await readFile(filename, 'utf8'), generated, path.basename(filename));
    }
  } finally {
    await rm(destination, { recursive: true, force: true });
  }
});
