import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repository = fileURLToPath(new URL('../../', import.meta.url));
const githubSource = 'https://github.com/ExaDev/pg_warren/blob/main/';

function rewriteLinks(text) {
  return text.replace(/(\]\()([^\s)]+)(\))/g, (match, open, target, close) => {
    if (/^(?:[a-z][a-z\d+.-]*:|\/|#)/i.test(target)) return match;
    const [filename, ...fragment] = target.split('#');
    if (!filename.endsWith('.md')) return match;
    const source = path.posix.normalize(`docs/${filename}`);
    const anchor = fragment.length ? `#${fragment.join('#')}` : '';
    return `${open}${githubSource}${source}${anchor}${close}`;
  });
}

// Code examples stay exactly as tested by test/sql/docs_examples.sql. Only
// prose links are adapted for this first, single-page documentation preview.
function rewriteProseLinks(markdown) {
  let fence;
  return markdown.split(/(?<=\n)/).map((line) => {
    const marker = line.match(/^ {0,3}(`{3,}|~{3,})/);
    if (fence) {
      if (marker && marker[1][0] === fence[0] && marker[1].length >= fence.length
          && line.slice(marker[0].length).trim() === '') fence = undefined;
      return line;
    }
    if (marker) {
      fence = marker[1];
      return line;
    }
    return line.split(/(`+[^`]*`+)/g).map((part) => part.startsWith('`') ? part : rewriteLinks(part)).join('');
  }).join('');
}

export function renderTraversal(markdown) {
  if (!markdown.startsWith('# Traversal\n')) {
    throw new Error('Expected docs/traversal.md to start with "# Traversal"');
  }
  const body = rewriteProseLinks(markdown.slice('# Traversal\n'.length));
  return `---
title: Traversal
description: Breadth-first search, reachability, shortest paths, and typed walks over your PostgreSQL graph versions.
editUrl: https://github.com/ExaDev/pg_warren/edit/main/docs/traversal.md
sidebar:
  badge:
    text: Development
    variant: caution
---

:::note[Development documentation]
This page follows the development branch. For a released version, consult the [release documentation](https://github.com/ExaDev/pg_warren/releases).
:::
${body}`;
}

export async function prepareDocs(sourceRoot = repository, destinationRoot = path.join(repository, 'site/src/content/docs')) {
  const markdown = await readFile(path.join(sourceRoot, 'docs/traversal.md'), 'utf8');
  const destination = path.join(destinationRoot, 'docs/traversal.md');
  await mkdir(path.dirname(destination), { recursive: true });
  await writeFile(destination, renderTraversal(markdown));
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await prepareDocs();
}
