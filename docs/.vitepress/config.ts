import { defineConfig } from 'vitepress'

// Deployed to GitHub Pages under the repository name.
const base = process.env.DOCS_BASE || '/naspeed/'

export default defineConfig({
  lang: 'en-US',
  title: 'naspeed',
  description:
    'Transparent RUSTC_WRAPPER: a content-addressed crate store cloned copy-on-write into every target/ dir, machine-wide rustc slots, and the ns project front-end.',
  base,

  // Examples reference tailnet and localhost peers that are not navigable
  // from the published site.
  ignoreDeadLinks: 'localhostLinks',

  cleanUrls: true,

  themeConfig: {
    nav: [
      { text: 'Guide', link: '/guide/getting-started' },
      { text: 'Configuration', link: '/guide/configuration' },
      { text: 'GitHub', link: 'https://github.com/dacsang97/naspeed' },
    ],
    sidebar: [
      {
        text: 'Guide',
        items: [
          { text: 'Getting started', link: '/guide/getting-started' },
          { text: 'Commands', link: '/guide/commands' },
          { text: 'ns front-end', link: '/guide/ns' },
          { text: 'Test result cache', link: '/guide/test-cache' },
          { text: 'Peer cache over Tailscale', link: '/guide/peer-cache' },
          { text: 'GitHub Actions', link: '/guide/github-actions' },
          { text: 'AI agents', link: '/guide/agents' },
        ],
      },
      {
        text: 'Internals',
        items: [
          { text: 'How it works', link: '/guide/how-it-works' },
          { text: 'Limits', link: '/guide/limits' },
        ],
      },
      {
        text: 'Reference',
        items: [
          { text: 'Configuration', link: '/guide/configuration' },
          { text: 'Development', link: '/guide/development' },
        ],
      },
    ],
    socialLinks: [{ icon: 'github', link: 'https://github.com/dacsang97/naspeed' }],
    editLink: {
      pattern: 'https://github.com/dacsang97/naspeed/edit/main/docs/:path',
      text: 'Edit this page on GitHub',
    },
    search: { provider: 'local' },
    footer: {
      message: 'Apache-2.0 licensed.',
    },
  },
})
