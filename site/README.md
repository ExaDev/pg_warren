# pg_warren website

An Astro homepage and Starlight documentation, built as static files for
`https://exadev.github.io/pg_warren/`. The website workflow builds a downloadable
review artifact; it does not yet deploy to Pages.
The existing signed apt repository and release workflows are unchanged.

## Run locally

Use Node.js 24 or later:

```sh
cd site
npm ci
npm run dev
```

Open the printed local URL with `/pg_warren/` appended. The documentation
starts at `/pg_warren/docs/`. The site supports light and dark themes. The
homepage includes copyable SQL and an expandable, self-contained setup.
Documentation has Pagefind search in a production preview. The importer runs
before `dev` and `build`; restart `dev` after editing canonical source files.

```sh
npm test
npm run build
npm run check:build
npm run preview
```

The build checker validates generated links, anchors, assets, canonical URLs,
and the search index under the project base path. Importer tests ensure SQL
blocks remain identical to the canonical documentation.

## Content and maintenance

- Edit `src/pages/index.astro` for homepage content and `src/styles/` for
  presentation. Fonts are bundled locally; the site needs no font service.
- `src/assets/exadev.svg` is the symbol from the ExaDev logo retrieved from
  `https://exadev.io/_next/static/media/header-logo.b17c8968.svg`, with the
  stacked wordmark removed and its view box cropped to the symbol. The header
  attribution links to `https://exadev.io` on the homepage and documentation.
- `scripts/prepare-docs.mjs` imports the canonical user guides from `../docs/`,
  installation/quickstart/comparisons from `../README.md`, and the changelog,
  benchmark methodology, and contributor guide. Its `generatedPages` registry
  declares routes and source files. Edit those originals, not their ignored
  generated pages. Original SQL blocks stay byte-identical and retain coverage
  in `../test/sql/docs_examples.sql`. Internal source links become site links;
  references to engineering material and result files stay on GitHub.
- `scripts/sql-reference.mjs` derives the software version from
  `[workspace.package]`, then generates grouped public function signatures from
  the matching `../sql/pg_warren--X.Y.Z.sql`. It excludes trigger helpers and
  fails on unclassified public function names so new APIs cannot silently
  disappear. This is a signature reference; the linked guides explain usage.
- The overview, operations guide, and architecture overview are tracked in
  `src/content/docs/docs/`. Edit these files directly. Their edit links lead to
  the website source, while imported guides lead to the original Markdown.
- Documentation explicitly follows development on `main`. The installation
  page compares the current workspace version with `../sql/released.commits`
  and always directs readers to published assets to verify package availability:
  a frozen release record does not mean publication has succeeded.
  A dedicated site tree for stable releases is part of the publication phase.
- The PR workflow uploads `website-preview`. To view a downloaded artifact,
  put its contents in a directory named `pg_warren`, serve the parent with a
  local HTTP server, and open `/pg_warren/`. Opening HTML through `file://`
  does not support the absolute project paths or search.

## Publication phase

Before deploying, implement the shared website/apt assembly path: persist a
verifiable signed apt tree, preserve the release and Pages approval gates, serialize all Pages
deployments before resolving source snapshots, and verify the combined size.
No independent website-only Pages deployment should replace the existing
package repository. Stable content will follow published releases; development
content must retain its label.
