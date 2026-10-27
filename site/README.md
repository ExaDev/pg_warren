# Website design preview

An Astro homepage and a Starlight traversal guide, built as static files for
`https://exadev.github.io/pg_warren/`. This first PR is the visual review
checkpoint. It builds a downloadable preview; it does not deploy to Pages.
The existing signed apt repository and release workflows are unchanged.

## Run locally

Use Node.js 24 or later:

```sh
cd site
npm ci
npm run dev
```

Open the printed local URL with `/pg_warren/` appended. The representative
documentation page is `/pg_warren/docs/traversal/`. Both pages support light
and dark themes. The homepage includes copyable SQL and an expandable,
self-contained setup. The guide has Pagefind search in a production preview.

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
- Edit `../docs/traversal.md` for the guide. `scripts/prepare-docs.mjs` generates
  its ignored Starlight page before development and builds. Do not edit the
  generated page. Its SQL remains covered by `../test/sql/docs_examples.sql`.
- This preview explicitly follows development on `main`. Links to guides not
  yet integrated go to their canonical GitHub sources. Installation links go
  to the README; this preview does not claim development packages are released.
- The PR workflow uploads `website-preview`. To view a downloaded artifact,
  put its contents in a directory named `pg_warren`, serve the parent with a
  local HTTP server, and open `/pg_warren/`. Opening HTML through `file://`
  does not support the absolute project paths or search.

## After visual approval

Integrate the remaining documentation, getting-started pages, SQL reference,
operations material, performance page, and release notes. Before deploying,
implement the shared website/apt assembly path: persist a verifiable signed
apt tree, preserve the release and Pages approval gates, serialize all Pages
deployments before resolving source snapshots, and verify the combined size.
No independent website-only Pages deployment should replace the existing
package repository. Stable content will follow published releases; development
content must retain its label.
