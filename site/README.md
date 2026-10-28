# pg_warren website

An Astro homepage and Starlight documentation, built as static files for
`https://exadev.github.io/pg_warren/`. Pull requests build a downloadable review
artifact. On `main`, the shared Pages workflow combines the website with the
latest published, verified apt snapshot and deploys both behind the existing
`github-pages` environment approval.

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
- The example library lives in `../docs/examples/`. Its `<!-- sql:file.sql -->`
  markers embed the exact scripts in `../examples/`; edit those scripts rather
  than copying SQL into Markdown. `scripts/examples.mjs` declares the pages
  and their script order, and generates complete downloads under
  `public/examples/`, including dataset setup for a fresh database. The same
  scripts run in `../test/sql/example_library.sql` with correctness assertions.
- Documentation explicitly follows development on `main`. The installation
  page compares the current workspace version with `../sql/released.commits`
  and always directs readers to published assets to verify package availability:
  a frozen release record does not mean publication has succeeded.
  Released source and documentation remain linked from those notices; the
  website currently presents development documentation.
- The PR workflow uploads `website-preview`. To view a downloaded artifact,
  put its contents in a directory named `pg_warren`, serve the parent with a
  local HTTP server, and open `/pg_warren/`. Opening HTML through `file://`
  does not support the absolute project paths or search.

## Publication

`.github/workflows/pages.yml` is the sole Pages deployment owner. It serialises
build and deployment, checks out fresh `main`, builds the site, and runs
`release/pages.py` to verify and assemble its apt snapshot. That build job has
no signing key. `release-publish.yml` attaches signed snapshots as immutable
GitHub Release assets only after its publishing steps succeed.

The first website can deploy without apt before any packaged release is
published. Once one exists, a missing or invalid snapshot blocks deployment;
the pipeline does not publish the website by itself. `deployment.json` records
the checked-out source SHA and apt version. The combined output must fit
within 1,000,000,000 bytes. See the [deployment procedures](../CONTRIBUTING.md#website-and-apt-deployment)
for trust checks, concurrency, reruns, partial-upload recovery, and approval.
