import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';

export const examplePages = [
  { slug: 'examples', source: 'docs/examples/index.md', title: 'Example library', description: 'Complete SQL walkthroughs for traversal, communities, semirings, taint analysis, and graph versions.' },
  { slug: 'examples/setup', source: 'docs/examples/setup.md', title: 'Build the example dataset', description: 'Start a published container and create a reproducible graph of 10000 people and 49985 edges.', sql: ['setup.sql'] },
  { slug: 'examples/bfs', source: 'docs/examples/bfs.md', title: 'Traverse from your application data', description: 'Look up starting people, traverse the graph, and join BFS results back to ordinary tables.', sql: ['bfs.sql'] },
  { slug: 'examples/communities', source: 'docs/examples/communities.md', title: 'Find communities with Louvain and Leiden', description: 'Find graph communities, compare their sizes, and inspect their members.', sql: ['communities.sql'] },
  { slug: 'examples/walk-counts', source: 'docs/examples/walk-counts.md', title: 'Count walks with a semiring', description: 'Use plus_times to count six-hop directed walks and rank their endpoints.', sql: ['walk-counts.sql'] },
  { slug: 'examples/min-plus', source: 'docs/examples/min-plus.md', title: 'Compute distances with min plus', description: 'Build shortest distances from semiring products and check every result against BFS.', sql: ['min-plus.sql'] },
  { slug: 'examples/boolean-spread', source: 'docs/examples/boolean-spread.md', title: 'Propagate a boolean through the graph', description: 'Spread an alert with a boolean semiring and inspect each node\'s reached flag.', sql: ['boolean-spread.sql'] },
  { slug: 'examples/taint', source: 'docs/examples/taint.md', title: 'Trace exposure to the nearest taint', description: 'Mark 100 tainted people, annotate shortest distances, and follow pointers back to a seed.', sql: ['taint-seeds.sql', 'taint-distance.sql', 'taint-routes.sql'] },
  { slug: 'examples/versions', source: 'docs/examples/versions.md', title: 'Try a change and roll it back', description: 'Query a staged graph version without changing the current graph, then undo the scenario.', sql: ['versions.sql'] },
];

// SQL files are shared by the site, downloads, and SQL test harness.
export async function expandExampleSql(markdown, sourceRoot) {
  for (const [marker, filename] of [...markdown.matchAll(/^<!-- sql:([a-z-]+\.sql) -->$/gm)]) {
    const sql = await readFile(path.join(sourceRoot, 'examples', filename), 'utf8');
    markdown = markdown.replace(marker, () => '```sql\n' + sql + '```');
  }
  return markdown;
}

export async function writeExampleDownloads(sourceRoot, directory) {
  await mkdir(directory, { recursive: true });
  const setup = await readFile(path.join(sourceRoot, 'examples/setup.sql'), 'utf8');
  for (const page of examplePages.filter(({ sql }) => sql)) {
    const filenames = page.sql.filter((filename) => filename !== 'setup.sql');
    const blocks = await Promise.all(filenames.map((filename) =>
      readFile(path.join(sourceRoot, 'examples', filename), 'utf8')));
    const name = page.slug.split('/').at(-1);
    await writeFile(path.join(directory, `${name}.sql`),
      // Commit setup before a recipe's explicit rollback, including when a
      // GUI submits the entire downloaded file as one multi-statement query.
      `-- ${page.title}\n-- Run this complete script in a fresh database.\n-- https://exadev.github.io/pg_warren/docs/${page.slug}/\n\nBEGIN;\n${setup}\nCOMMIT;\n\n${blocks.join('\n')}`);
  }
}
