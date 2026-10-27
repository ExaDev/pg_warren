import { mkdir, readFile, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const repository = fileURLToPath(new URL('../../', import.meta.url));

// A small SQL lexer, rather than a declaration regex: function bodies, comments,
// strings, and dollar-quoted defaults may all contain apparent declarations.
function tokens(sql) {
  const result = [];
  let offset = 0;
  while (offset < sql.length) {
    if (/\s/.test(sql[offset])) { offset += 1; continue; }
    if (sql.startsWith('--', offset)) {
      const end = sql.indexOf('\n', offset);
      offset = end < 0 ? sql.length : end + 1;
      continue;
    }
    if (sql.startsWith('/*', offset)) {
      let depth = 1;
      offset += 2;
      while (offset < sql.length && depth) {
        if (sql.startsWith('/*', offset)) { depth += 1; offset += 2; }
        else if (sql.startsWith('*/', offset)) { depth -= 1; offset += 2; }
        else offset += 1;
      }
      if (depth) throw new Error('Unterminated SQL block comment');
      continue;
    }
    const start = offset;
    const delimiter = sql.slice(offset).match(/^\$(?:[A-Za-z_][A-Za-z_\d]*)?\$/)?.[0];
    if (delimiter) {
      const end = sql.indexOf(delimiter, offset + delimiter.length);
      if (end < 0) throw new Error('Unterminated SQL dollar quote');
      offset = end + delimiter.length;
    } else if (sql[offset] === "'" || sql[offset] === '"') {
      const quote = sql[offset++];
      const escaped = quote === "'" && /^[eE]$/.test(result.at(-1)?.text ?? '')
        && result.at(-1).end === start;
      let closed = false;
      while (offset < sql.length) {
        if (escaped && sql[offset] === '\\') { offset += 2; continue; }
        if (sql[offset++] !== quote) continue;
        if (sql[offset] === quote) { offset += 1; continue; }
        closed = true;
        break;
      }
      if (!closed) throw new Error('Unterminated SQL quoted value');
    } else {
      const word = sql.slice(offset).match(/^[A-Za-z_][A-Za-z_\d$]*|^\d+(?:\.\d+)?(?:[eE][+-]?\d+)?|^::/);
      offset += word ? word[0].length : 1;
    }
    const text = sql.slice(start, offset);
    result.push({ text, end: offset, keyword: /^[A-Za-z_][A-Za-z_\d$]*$/.test(text) ? text.toUpperCase() : '' });
  }
  return result;
}

function format(parts) {
  let output = '';
  for (let index = 0; index < parts.length; index += 1) {
    const current = parts[index].text;
    const previous = parts[index - 1]?.text;
    const adjacent = !index || [',', ')', ']', '.', '::', '['].includes(current)
      || ['(', '[', '.', '::'].includes(previous)
      || (/^[eE]$/.test(previous ?? '') && current.startsWith("'"));
    output += `${adjacent ? '' : ' '}${current}`;
  }
  return output;
}

const identifier = (token) => token.text.startsWith('"')
  ? token.text.slice(1, -1).replaceAll('""', '"') : token.text.toLowerCase();
const attributes = new Set(['LANGUAGE', 'AS', 'IMMUTABLE', 'STABLE', 'VOLATILE', 'STRICT', 'CALLED', 'RETURNS',
  'SECURITY', 'PARALLEL', 'COST', 'ROWS', 'SUPPORT', 'SET', 'WINDOW', 'LEAKPROOF', 'NOT', 'BEGIN']);

export function parseSqlFunctions(sql) {
  const lexed = tokens(sql);
  const functions = [];
  for (let start = 0; start < lexed.length; start += 1) {
    if (lexed[start].keyword !== 'CREATE') continue;
    let cursor = start + 1;
    if (lexed[cursor]?.keyword === 'OR' && lexed[cursor + 1]?.keyword === 'REPLACE') cursor += 2;
    if (lexed[cursor]?.keyword !== 'FUNCTION') continue;
    cursor += 1;
    const schema = lexed[cursor++];
    if (!schema || lexed[cursor++]?.text !== '.') continue;
    const name = lexed[cursor++];
    if (!name || lexed[cursor++]?.text !== '(') continue;
    const argumentParts = [];
    let argument = [];
    let depth = 0;
    while (cursor < lexed.length) {
      const token = lexed[cursor++];
      if (token.text === ')' && depth === 0) {
        if (argument.length) argumentParts.push(format(argument));
        break;
      }
      if (token.text === ',' && depth === 0) {
        argumentParts.push(format(argument));
        argument = [];
        continue;
      }
      if (['(', '['].includes(token.text)) depth += 1;
      if ([')', ']'].includes(token.text)) depth -= 1;
      argument.push(token);
    }
    if (lexed[cursor++]?.keyword !== 'RETURNS') throw new Error(`Missing RETURNS for ${name.text}`);
    const returns = [];
    depth = 0;
    while (cursor < lexed.length) {
      const token = lexed[cursor];
      if (depth === 0 && (attributes.has(token.keyword) || token.text === ';')) break;
      cursor += 1;
      if (['(', '['].includes(token.text)) depth += 1;
      if ([')', ']'].includes(token.text)) depth -= 1;
      returns.push(token);
    }
    const resultType = format(returns);
    if (!resultType) throw new Error(`Missing return type for ${name.text}`);
    // PostgreSQL grants EXECUTE to PUBLIC by default. This install SQL revokes
    // it only from catalog trigger helpers; event triggers are likewise not a
    // direct-call API. Privilege checks inside public functions still apply.
    if (identifier(schema) !== 'warren' || /^(?:event_)?trigger$/i.test(resultType)) continue;
    functions.push({
      name: identifier(name), qualifiedName: `${schema.text}.${name.text}`,
      arguments: argumentParts, returns: resultType,
    });
  }
  return functions;
}

export function workspaceVersion(toml) {
  const section = toml.match(/^\[workspace\.package\]\s*\n([\s\S]*?)(?=^\[|$(?![\s\S]))/m)?.[1];
  const version = section?.match(/^version\s*=\s*"([\d]+\.[\d]+\.[\d]+(?:[-+][\w.-]+)?)"\s*(?:#.*)?$/m)?.[1];
  if (!version) throw new Error('Missing version in Cargo.toml [workspace.package]');
  return version;
}

const groups = [
  ['Graphs', 'Define a graph from SQL queries and manage its lifetime.', 'semantics', 'create_graph drop_graph info'],
  ['Versions', 'Build, stage, promote, compare, and compact immutable graph versions.', 'change-detection', 'refresh promote drop_version compact diff'],
  ['Traversal', 'Lookups, breadth-first search, reachability, paths, components, and typed walk steps.', 'traversal', 'bfs neighbours degree has_edge reachable reachable_set shortest_path dijkstra components walk s steps last_stats'],
  ['Vectors and matrices', 'Construct, inspect, and remove named per-node vectors and matrices.', 'compute', 'vector_from_query vector_const vector_degree vector_dense_id vector_values matrix_rows vector_topk vector_stats id_map drop_vector'],
  ['Compute primitives', 'Semiring products, SQL expressions, reductions, and SQL-typed messages.', 'compute', 'spmv spmm vector_map explain_map vector_reduce vector_norm_diff gather'],
  ['Algorithms', 'Graph algorithms built over the same versions and compute primitives.', 'compute', 'pagerank eigenvector_centrality katz_centrality hits components_vector label_propagation label_propagation_majority sssp_vector degree_vector aggregate_neighbours triangles clustering core_number strong_components_vector topological_order dag_longest_path minimum_spanning_forest betweenness_centrality closeness_centrality harmonic_centrality similarity most_similar similar_pairs'],
  ['Spectral analysis', 'Eigenpairs and structural, spectral, or embedding distances between graphs or versions.', 'compute', 'eigen distance'],
  ['Communities', 'Community detection, contraction, projection, and lifting between levels.', 'communities', 'contract project lift modularity local_moving leiden_refine louvain leiden'],
  ['Access and ownership', 'Graph access, graph and vector ownership, quotas, and cleanup after role removal.', 'semantics', 'grant revoke set_owner set_vector_quota vector_usage reassign_owned drop_owned orphans drop_orphans'],
  ['Cache and operations', 'Load and evict versions, inspect cache state, and observe running work.', 'operations', 'prewarm evict cache shared_cache backends progress report_progress'],
  ['Extension', 'Inspect the installed extension software version.', 'operations', 'version'],
];

export function renderSqlReference(functions, version) {
  const byName = new Map();
  for (const fn of functions) {
    if (!groups.some(([, , , names]) => names.split(' ').includes(fn.name))) {
      throw new Error(`Unclassified public SQL function: ${fn.name}; add it to sql-reference.mjs`);
    }
    if (!byName.has(fn.name)) byName.set(fn.name, []);
    byName.get(fn.name).push(fn);
  }
  const source = `sql/pg_warren--${version}.sql`;
  let output = `---
title: SQL function reference
description: Public SQL signatures, defaults, and return types generated from the current pg_warren install script.
editUrl: https://github.com/ExaDev/pg_warren/edit/main/site/scripts/sql-reference.mjs
---

:::note[Development reference]
Software version **${version} · development**. These signatures are generated from the [current install SQL](https://github.com/ExaDev/pg_warren/blob/main/${source}), rather than maintained separately. For released software, use its tagged install SQL and release notes.
:::

Functions are in schema \`warren\`. Signatures below preserve the SQL argument names, types, defaults, overloads, and return types. Named arguments use \`argument := value\`. Graph access and ownership checks still apply; see [access rules](/pg_warren/docs/semantics/#access).

An \`anyelement\` or \`anyarray\` argument uses the graph's external id type. Cast ids explicitly, such as \`1::bigint\`. Where an \`id_type\` argument supplies a type witness, use a typed null such as \`NULL::bigint\`. See [id types and type witnesses](/pg_warren/docs/semantics/#id-types-and-type-witnesses).

## Inspect your installation

The server is authoritative for the software you have installed. In psql, list its functions with:

\`\`\`text
\\df+ warren.*
\`\`\`

Check the installed software version with \`SELECT warren.version();\`. Function results that name graph versions refer to immutable graph builds, rather than the extension's software version. Catalog trigger and event-trigger helpers are excluded because they cannot be called as ordinary functions.
`;
  for (const [title, description, page, names] of groups) {
    const present = names.split(' ').filter((name) => byName.has(name));
    if (!present.length) continue;
    output += `\n## ${title}\n\n${description} [Read the guide](/pg_warren/docs/${page}/).\n`;
    for (const name of present) {
      output += `\n### \`warren.${name}\`\n`;
      for (const fn of byName.get(name)) {
        const argumentsText = fn.arguments.length ? `\n  ${fn.arguments.join(',\n  ')}\n` : '';
        output += `\n\`\`\`sql\n${fn.qualifiedName}(${argumentsText})\nRETURNS ${fn.returns};\n\`\`\`\n`;
      }
    }
  }
  return output;
}

export async function prepareSqlReference(sourceRoot = repository, destinationRoot = path.join(sourceRoot, 'site/src/content/docs')) {
  const version = workspaceVersion(await readFile(path.join(sourceRoot, 'Cargo.toml'), 'utf8'));
  const sql = await readFile(path.join(sourceRoot, `sql/pg_warren--${version}.sql`), 'utf8');
  const output = renderSqlReference(parseSqlFunctions(sql), version);
  const destination = path.join(destinationRoot, 'docs/reference.md');
  await mkdir(path.dirname(destination), { recursive: true });
  await writeFile(destination, output);
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  await prepareSqlReference();
}
