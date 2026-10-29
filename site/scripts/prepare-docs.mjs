import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { prepareSqlReference, workspaceVersion } from './sql-reference.mjs';
import { examplePages, expandExampleSql, writeExampleDownloads } from './examples.mjs';

const repository = fileURLToPath(new URL('../../', import.meta.url));
const githubSource = 'https://github.com/ExaDev/pg_warren/blob/main/';
const docsRoute = '/pg_warren/docs/';

export const generatedPages = [
  { slug: 'semantics', source: 'docs/semantics.md', title: 'Graphs and versions', description: 'Graph definitions, versions, ids, transactions, access, and freshness.' },
  { slug: 'traversal', source: 'docs/traversal.md', title: 'Traversal', description: 'Breadth-first search, reachability, shortest paths, and typed walks.' },
  { slug: 'compute', source: 'docs/compute.md', title: 'Whole-graph computation', description: 'Vectors, semirings, SQL expressions, graph algorithms, and spectral analysis.' },
  { slug: 'communities', source: 'docs/communities.md', title: 'Communities', description: 'Community detection, contraction, projection, and lifting between graph levels.' },
  { slug: 'change-detection', source: 'docs/change-detection.md', title: 'Change detection', description: 'Compare structural change inside a transaction or with a shadow graph.' },
  { slug: 'limits', source: 'docs/limits.md', title: 'Settings and limits', description: 'Configuration, memory, shared cache, sizing, and known limitations.' },
  { slug: 'security', source: 'docs/security.md', title: 'Security and operations', description: 'What pg_warren installs, privileges, failure model, resource limits, test evidence, and releases.' },
  { slug: 'beside-managed', source: 'docs/beside-managed.md', title: 'Beside a managed database', description: 'Run pg_warren on a server of your own next to Amazon RDS or another managed database, fed by logical replication or postgres_fdw.' },
  { slug: 'installation', source: 'README.md', section: 'Install', title: 'Installation', description: 'Install pg_warren, configure PostgreSQL, and upgrade an existing installation.' },
  { slug: 'quickstart', source: 'README.md', section: 'A ten-line example', title: 'Your first graph', description: 'Create a graph from ordinary tables, traverse it, and compute PageRank.' },
  { slug: 'performance', source: 'README.md', section: 'How it compares', title: 'Performance and comparisons', description: 'Feature comparisons and measured graph workloads, with their caveats.' },
  { slug: 'positioning', source: 'docs/positioning.md', title: 'Where pg_warren fits', description: 'How pg_warren compares with pgGraph and the wider PostgreSQL graph landscape, and when to choose each.' },
  { slug: 'changelog', source: 'CHANGELOG.md', title: 'Changelog', description: 'Development changes, release history, and upgrade instructions.' },
  { slug: 'benchmarks', source: 'bench/results/corpus/head_to_head/README.md', title: 'Benchmark methodology', description: 'Head-to-head benchmark provenance, settings, results, and caveats.' },
  { slug: 'contributing', source: 'CONTRIBUTING.md', title: 'Contributing', description: 'Build, test, benchmark, and contribute to pg_warren.' },
  ...examplePages,
];

const readmeAnchors = new Map([
  ['install', 'installation'],
  ['a-ten-line-example', 'quickstart'],
  ['how-it-compares', 'performance'],
  ['features', 'performance'],
  ['performance', 'performance'],
]);

export function routeForSource(source, fragment = '') {
  const anchor = fragment ? `#${fragment}` : '';
  if (source === 'README.md') {
    if (!fragment || fragment === 'documentation') return docsRoute;
    const slug = readmeAnchors.get(fragment);
    return slug ? `${docsRoute}${slug}/${anchor}` : `${githubSource}${source}${anchor}`;
  }
  const page = generatedPages.find((candidate) => candidate.source === source);
  return page ? `${docsRoute}${page.slug}/${anchor}` : `${githubSource}${source}${anchor}`;
}

function rewriteLinks(text, page) {
  return text.replace(/(\]\()([^\s)]+)(\))/g, (match, open, target, close) => {
    if (/^(?:[a-z][a-z\d+.-]*:|\/)/i.test(target)) return match;
    const [filename, ...fragments] = target.split('#');
    const fragment = fragments.join('#');
    if (!filename && page.source !== 'README.md') return match;
    const source = filename ? path.posix.normalize(path.posix.join(path.posix.dirname(page.source), filename)) : page.source;
    return `${open}${routeForSource(source, fragment)}${close}`;
  });
}

// Canonical examples are tested by test/sql/docs_examples.sql. Only prose links
// are adapted, leaving fenced examples and inline code byte-for-byte unchanged.
function rewriteProseLinks(markdown, page) {
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
    return line.split(/(`+[^`]*`+)/g).map((part) => part.startsWith('`') ? part : rewriteLinks(part, page)).join('');
  }).join('');
}

export function extractSection(markdown, title) {
  const lines = markdown.split(/(?<=\n)/);
  const start = lines.findIndex((line) => line.trimEnd() === `## ${title}`);
  if (start === -1) throw new Error(`Missing README section: ${title}`);
  let end = start + 1;
  while (end < lines.length && !/^## /.test(lines[end])) end++;
  return lines.slice(start, end).join('');
}

function headingAnchor(title) {
  return title.toLowerCase().replace(/[^\p{L}\p{N}\s_-]/gu, '').replace(/\s/g, '-');
}

export function renderPage(markdown, page, status = {}) {
  const heading = markdown.match(/^#{1,2} (.+)\n/);
  if (!heading) throw new Error(`Expected an opening Markdown heading in ${page.source}`);
  const body = rewriteProseLinks(markdown.slice(heading[0].length), page);
  const versionStatus = status.current && status.latest && status.current !== status.latest
    ? `This checkout develops **${status.current}**; the latest recorded release is **${status.latest}**. ` : '';
  const availability = page.slug === 'installation' ? `
:::caution[Package availability]
${versionStatus}Package channels begin with 0.7.0. A recorded release marks frozen source; it does not establish that release approvals have completed or that apt, Docker, or Homebrew packages are live. The canonical instructions below may describe an upcoming release: consult the [published release notes and assets](https://github.com/ExaDev/pg_warren/releases) before assuming a package or image is available. Follow the signing-key verification instructions before using the apt repository.
:::
` : '';
  const download = page.sql ? `\n[Download the complete SQL script](/pg_warren/examples/${page.slug.split('/').at(-1)}.sql), including dataset setup for a fresh database.\n` : '';
  return `---
title: ${JSON.stringify(page.title)}
description: ${JSON.stringify(page.description || page.title)}
editUrl: ${JSON.stringify(`https://github.com/ExaDev/pg_warren/edit/main/${page.source}`)}
---

:::note[Development documentation]
This page follows the development branch. For the software and documentation of a released version, consult the [published releases](https://github.com/ExaDev/pg_warren/releases).
:::
${availability}
${download}
<span id="${headingAnchor(heading[1])}"></span>
${body}`;
}

export async function prepareDocs(sourceRoot = repository, destinationRoot = path.join(repository, 'site/src/content/docs'), { reference = true, downloads = path.join(sourceRoot, 'site/public/examples') } = {}) {
  const manifest = await readFile(path.join(sourceRoot, 'Cargo.toml'), 'utf8');
  const releases = await readFile(path.join(sourceRoot, 'sql/released.commits'), 'utf8');
  const current = workspaceVersion(manifest);
  const latest = releases.trim().split(/\n/).at(-1)?.trim().split(/\s+/).at(-1);
  if (!current || !latest) throw new Error('Cannot determine development and recorded release versions');
  await mkdir(path.join(destinationRoot, 'docs'), { recursive: true });
  for (const page of generatedPages) {
    const source = await readFile(path.join(sourceRoot, page.source), 'utf8');
    const markdown = await expandExampleSql(page.section ? extractSection(source, page.section) : source, sourceRoot);
    const destination = path.join(destinationRoot, 'docs', `${page.slug}.md`);
    await mkdir(path.dirname(destination), { recursive: true });
    await writeFile(destination, renderPage(markdown, page, { current, latest }));
  }
  if (downloads) await writeExampleDownloads(sourceRoot, downloads);
  if (reference) {
    await prepareSqlReference(sourceRoot, destinationRoot);
  }
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await prepareDocs();
}
