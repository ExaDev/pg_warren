import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';

export default defineConfig({
  site: 'https://exadev.github.io',
  base: '/pg_warren',
  trailingSlash: 'always',
  integrations: [starlight({
    title: 'pg_warren',
    logo: { src: './src/assets/mark.svg' },
    description: 'Deep traversal and whole-graph computation for PostgreSQL.',
    customCss: ['./src/styles/docs.css'],
    components: { SiteTitle: './src/components/SiteTitle.astro' },
    social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/ExaDev/pg_warren' }],
    sidebar: [
      { label: 'pg_warren home', link: '/' },
      { label: 'Start here', items: [
        { label: 'Documentation overview', link: '/docs/' },
        { slug: 'docs/positioning' },
        { slug: 'docs/installation' },
        { slug: 'docs/quickstart' },
      ] },
      { label: 'Concepts and guides', items: [
        { slug: 'docs/semantics' },
        { slug: 'docs/traversal' },
        { slug: 'docs/compute' },
        { slug: 'docs/communities' },
        { slug: 'docs/change-detection' },
      ] },
      { label: 'Example library', items: [
        { slug: 'docs/examples' },
        { slug: 'docs/examples/setup' },
        { slug: 'docs/examples/bfs' },
        { slug: 'docs/examples/communities' },
        { slug: 'docs/examples/walk-counts' },
        { slug: 'docs/examples/min-plus' },
        { slug: 'docs/examples/boolean-spread' },
        { slug: 'docs/examples/taint' },
        { slug: 'docs/examples/versions' },
      ] },
      { label: 'Reference and operations', items: [
        { slug: 'docs/reference' },
        { slug: 'docs/limits' },
        { slug: 'docs/operations' },
        { slug: 'docs/beside-managed' },
      ] },
      { label: 'Project', items: [
        { slug: 'docs/performance' },
        { slug: 'docs/benchmarks' },
        { slug: 'docs/changelog' },
        { slug: 'docs/architecture' },
        { slug: 'docs/contributing' },
      ] },
    ],
    editLink: { baseUrl: 'https://github.com/ExaDev/pg_warren/edit/main/' },
  })],
});
