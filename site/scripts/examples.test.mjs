import assert from 'node:assert/strict';
import { mkdtemp, readFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { test } from 'node:test';
import { examplePages, expandExampleSql, writeExampleDownloads } from './examples.mjs';

const repository = fileURLToPath(new URL('../../', import.meta.url));

test('example pages embed the exact runnable SQL without rewriting its contents', async () => {
  for (const page of examplePages) {
    const markdown = await readFile(path.join(repository, page.source), 'utf8');
    const rendered = await expandExampleSql(markdown, repository);
    assert.doesNotMatch(rendered, /<!-- sql:/);
    for (const filename of page.sql ?? []) {
      const sql = await readFile(path.join(repository, 'examples', filename), 'utf8');
      assert.ok(rendered.includes('```sql\n' + sql + '```'), `${page.slug}: ${filename}`);
    }
  }
});

test('every recipe download contains setup and all its SQL prerequisites in execution order', async () => {
  const directory = await mkdtemp(path.join(tmpdir(), 'pg-warren-examples-'));
  try {
    await writeExampleDownloads(repository, directory);
    const setup = await readFile(path.join(repository, 'examples/setup.sql'), 'utf8');
    for (const page of examplePages.filter(({ sql }) => sql)) {
      const name = page.slug.split('/').at(-1);
      const download = await readFile(path.join(directory, `${name}.sql`), 'utf8');
      assert.ok(download.includes(setup), name);
      assert.ok(download.includes(`BEGIN;\n${setup}\nCOMMIT;`), 'setup commits before any scenario rollback in a GUI batch');
      let previous = -1;
      for (const filename of page.sql) {
        const sql = await readFile(path.join(repository, 'examples', filename), 'utf8');
        const offset = download.indexOf(sql);
        assert.ok(offset > previous, `${name}: ${filename} must follow its prerequisites`);
        previous = offset;
      }
      assert.doesNotMatch(download, /^\\/m, 'downloads also run in pgAdmin');
    }
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
