import { readdir, readFile, stat } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const siteUrl = new URL('https://exadev.github.io/pg_warren/');
const defaultDirectory = fileURLToPath(new URL('../dist/', import.meta.url));

function entities(value) {
  const named = { amp: '&', quot: '"', apos: "'", lt: '<', gt: '>' };
  return value.replace(/&(#x[0-9a-f]+|#\d+|amp|quot|apos|lt|gt);/gi, (_, entity) => {
    if (entity.startsWith('#')) {
      const number = entity[1].toLowerCase() === 'x'
        ? Number.parseInt(entity.slice(2), 16)
        : Number.parseInt(entity.slice(1), 10);
      return String.fromCodePoint(number);
    }
    return named[entity.toLowerCase()];
  });
}

function tags(html) {
  // Ignore comments and executable text so strings containing HTML are not links.
  const markup = html.replace(/<!--[\s\S]*?-->/g, '').replace(
    /(<(script|style)\b[^>]*>)[\s\S]*?<\/\2\s*>/gi,
    '$1',
  );
  return [...markup.matchAll(/<([a-z][\w:-]*)\b([^<>]*?)>/gi)].map((match) => {
    const attributes = {};
    for (const attribute of match[2].matchAll(/([^\s=/'">]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s'"=<>`]+)))?/g)) {
      attributes[attribute[1].toLowerCase()] = entities(attribute[2] ?? attribute[3] ?? attribute[4] ?? '');
    }
    return { name: match[1].toLowerCase(), attributes };
  });
}

async function filesIn(directory, prefix = '') {
  const entries = await readdir(directory, { withFileTypes: true });
  const results = await Promise.all(entries.map(async (entry) => {
    const relative = path.posix.join(prefix, entry.name);
    return entry.isDirectory() ? filesIn(path.join(directory, entry.name), relative) : [relative];
  }));
  return results.flat();
}

function pagePath(file) {
  return file.replace(/(^|\/)index\.html$/, '$1');
}

export async function checkBuild(directory = defaultDirectory) {
  const files = await filesIn(directory);
  const available = new Set(files);
  const documents = new Map();
  const errors = [];
  for (const file of files.filter((name) => name.endsWith('.html'))) {
    const elements = tags(await readFile(path.join(directory, file), 'utf8'));
    documents.set(file, {
      elements,
      ids: new Set(elements.flatMap(({ attributes }) => [attributes.id, attributes.name].filter(Boolean))),
    });
  }

  for (const file of ['index.html', 'docs/traversal/index.html']) {
    const document = documents.get(file);
    if (!document) {
      errors.push(`Missing required page: ${file}`);
      continue;
    }
    const canonical = document.elements.filter(({ name, attributes }) =>
      name === 'link' && attributes.rel?.split(/\s+/).includes('canonical'));
    const expected = new URL(pagePath(file), siteUrl).href;
    if (canonical.length !== 1 || canonical[0].attributes.href !== expected) {
      errors.push(`${file}: expected one canonical link to ${expected}`);
    }
  }

  for (const [file, document] of documents) {
    const current = new URL(pagePath(file), siteUrl);
    for (const { name, attributes } of document.elements) {
      for (const attribute of ['href', 'src']) {
        const value = attributes[attribute];
        if (!value) continue;
        // Canonicals describe preferred URLs, not fetchable resources. Astro's
        // 404.html declares /404/ even though only 404.html is emitted. Required
        // content pages have their canonical URLs checked explicitly above.
        if (attribute === 'href' && name === 'link'
          && attributes.rel?.split(/\s+/).includes('canonical')) continue;
        let target;
        try {
          target = new URL(value, current);
        } catch {
          errors.push(`${file}: invalid ${attribute} ${value}`);
          continue;
        }
        if (target.origin !== siteUrl.origin || !['http:', 'https:'].includes(target.protocol)) continue;
        if (!target.pathname.startsWith(siteUrl.pathname)) {
          errors.push(`${file}: ${attribute} escapes project base: ${value}`);
          continue;
        }
        let relative;
        let fragment;
        try {
          relative = decodeURIComponent(target.pathname.slice(siteUrl.pathname.length));
          fragment = decodeURIComponent(target.hash.slice(1));
        } catch {
          errors.push(`${file}: invalid URL encoding in ${value}`);
          continue;
        }
        const candidates = [relative, `${relative.replace(/\/$/, '')}/index.html`];
        if (!relative) candidates.push('index.html');
        const destination = candidates.find((candidate) => available.has(candidate));
        if (!destination) {
          errors.push(`${file}: missing ${attribute} target ${value}`);
          continue;
        }
        if (fragment && fragment !== 'top' && documents.has(destination)
          && !documents.get(destination).ids.has(fragment)) {
          errors.push(`${file}: missing anchor ${value}`);
        }
      }
    }
  }

  if (!available.has('pagefind/pagefind.js') || !files.some((file) => /^pagefind\/.*\.pf_fragment$/.test(file))) {
    errors.push('Missing generated Pagefind search script or indexed document fragments');
  }
  if (errors.length) throw new Error(`Website build checks failed:\n${errors.map((error) => `- ${error}`).join('\n')}`);
  return { pages: documents.size, files: files.length };
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  try {
    const directory = process.argv[2] ? path.resolve(process.argv[2]) : defaultDirectory;
    if (!(await stat(directory)).isDirectory()) throw new Error(`Not a directory: ${directory}`);
    const result = await checkBuild(directory);
    console.log(`Website checked: ${result.pages} pages, ${result.files} files, project links and search present.`);
  } catch (error) {
    console.error(error.message);
    process.exitCode = 1;
  }
}
