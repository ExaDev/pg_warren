import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { test } from 'node:test';
import { checkBuild } from './check-build.mjs';
import { generatedPages } from './prepare-docs.mjs';

const expectedPages = ['index.html', 'docs/index.html',
  ...generatedPages.map(({ slug }) => `docs/${slug}/index.html`),
  ...['operations', 'architecture', 'reference'].map((slug) => `docs/${slug}/index.html`)];

async function fixture(run) {
  const directory = await mkdtemp(path.join(tmpdir(), 'pg-warren-build-'));
  const write = async (file, contents) => {
    await mkdir(path.dirname(path.join(directory, file)), { recursive: true });
    await writeFile(path.join(directory, file), contents);
  };
  try {
    for (const file of expectedPages) {
      const route = file.replace(/(^|\/)index\.html$/, '$1');
      await write(file, `<link rel="canonical" href="https://exadev.github.io/pg_warren/${route}">`);
    }
    await write('index.html', '<link rel="canonical" href="https://exadev.github.io/pg_warren/"><a href="/pg_warren/docs/traversal/#paths">Paths</a><img src="./mark.svg">');
    await write('docs/traversal/index.html', '<link rel="canonical" href="https://exadev.github.io/pg_warren/docs/traversal/"><h2 id="paths">Paths</h2>');
    await write('mark.svg', '<svg/>');
    await write('pagefind/pagefind.js', '');
    await write('pagefind/fragments/example.pf_fragment', '');
    await run(directory, write);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
}

test('checks project-prefixed pages, local assets, anchors, and search', async () => {
  await fixture(async (directory) => {
    assert.equal((await checkBuild(directory)).pages, expectedPages.length);
  });
});

test('accepts Astro 404 canonical metadata without requiring a 404 directory', async () => {
  await fixture(async (directory, write) => {
    await write('404.html', '<link rel="canonical" href="https://exadev.github.io/pg_warren/404/"><a href="/pg_warren/">Home</a>');
    assert.equal((await checkBuild(directory)).pages, expectedPages.length + 1);
  });
});

test('rejects missing local assets and links escaping the project base', async () => {
  await fixture(async (directory, write) => {
    await write('404.html', '<img src="/pg_warren/missing.svg"><a href="/docs/traversal/">Bad base</a>');
    await assert.rejects(checkBuild(directory), /missing src target[\s\S]*escapes project base/);
  });
});

test('rejects broken anchors and incorrect required canonical URLs', async () => {
  await fixture(async (directory, write) => {
    await write('index.html', '<link rel="canonical" href="https://exadev.github.io/"><a href="/pg_warren/docs/traversal/#missing">Bad anchor</a>');
    await assert.rejects(checkBuild(directory), /expected one canonical link[\s\S]*missing anchor/);
  });
});

test('rejects missing search data', async () => {
  await fixture(async (directory) => {
    await rm(path.join(directory, 'pagefind'), { recursive: true });
    await assert.rejects(checkBuild(directory), /Missing generated Pagefind/);
  });
});

test('requires every generated documentation page rather than only linked pages', async () => {
  await fixture(async (directory) => {
    await rm(path.join(directory, 'docs/installation'), { recursive: true });
    await assert.rejects(checkBuild(directory), /Missing required page: docs\/installation\/index\.html/);
  });
});

test('checks canonical URLs on additional HTML pages too', async () => {
  await fixture(async (directory, write) => {
    await write('extra/index.html', '<link rel="canonical" href="https://exadev.github.io/pg_warren/">');
    await assert.rejects(checkBuild(directory), /extra\/index\.html: expected one canonical link to https:\/\/exadev\.github\.io\/pg_warren\/extra\//);
  });
});

test('rejects missing and duplicate canonical metadata on documentation pages', async () => {
  await fixture(async (directory, write) => {
    await write('docs/operations/index.html', '<h1>Operations</h1>');
    await write('docs/architecture/index.html', '<link rel="canonical" href="https://exadev.github.io/pg_warren/docs/architecture/"><link rel="canonical" href="https://exadev.github.io/pg_warren/docs/architecture/">');
    await assert.rejects(checkBuild(directory), (error) => {
      assert.match(error.message, /docs\/operations\/index\.html: expected one canonical link/);
      assert.match(error.message, /docs\/architecture\/index\.html: expected one canonical link/);
      return true;
    });
  });
});
