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
      { label: 'Overview', link: '/' },
      { label: 'Guides · development', items: [{ slug: 'docs/traversal' }] },
      { label: 'All documentation ↗', link: 'https://github.com/ExaDev/pg_warren#documentation' },
    ],
    editLink: { baseUrl: 'https://github.com/ExaDev/pg_warren/edit/main/' },
  })],
});
