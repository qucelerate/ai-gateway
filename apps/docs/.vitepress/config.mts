import { defineConfig } from 'vitepress'
import { writeLlmsFiles } from '../scripts/llms.mjs'

// The public site: GitHub Pages at this host (public/CNAME), served from /.
const SITE = 'https://gateway.qucelerate.com'
const REPOSITORY = 'https://github.com/qucelerate/ai-gateway'

export default defineConfig({
  title: 'QuCelerate AI Gateway',
  description: 'Documentation for QuCelerate AI Gateway, a self-hosted, open-source LLM gateway with budgets, rate limits, catalog releases and tracing.',
  base: '/',
  cleanUrls: true,
  lastUpdated: true,
  buildConcurrency: 1,
  // The repository's contributor notes, not a page.
  srcExclude: ['README.md'],
  sitemap: { hostname: SITE },
  // Code blocks sit on --vp-c-bg-alt, where github-light's keyword red and
  // github-dark's comment grey are under 4.5:1; their high-contrast variants
  // meet WCAG AA there.
  markdown: { theme: { light: 'github-light-high-contrast', dark: 'github-dark-high-contrast' } },
  // Each page names its Markdown copy, for tools that read the site.
  transformHead: ({ pageData }) => [
    ['link', { rel: 'alternate', type: 'text/markdown', href: `/${pageData.relativePath}` }]
  ],
  // /llms.txt, /llms-full.txt and a .md copy of every page (scripts/llms.mjs).
  buildEnd: siteConfig => {
    writeLlmsFiles({
      srcDir: siteConfig.srcDir,
      outDir: siteConfig.outDir,
      pages: siteConfig.pages,
      sidebar: siteConfig.site.themeConfig.sidebar
    })
  },
  themeConfig: {
    nav: [
      { text: 'Install', link: '/install/agents' },
      { text: 'Guide', link: '/guide/' },
      { text: 'Architecture', link: '/architecture/' },
      { text: 'Quickstarts', link: '/quickstarts/' },
      { text: 'Reference', link: '/reference/' },
      { text: 'Clients', link: '/clients/' },
      { text: 'Operations', link: '/operations/' },
      { text: 'Support', link: '/support/' },
      // Version navigation: the contract these pages describe.
      {
        text: 'v1 (stable)',
        items: [
          { text: 'Contract 1.0.0', link: '/reference/compatibility' },
          { text: 'OpenAPI artifact', link: '/openapi/inference-v1.json', target: '_blank' },
          { text: 'Releases & upgrades', link: '/troubleshooting/releases' }
        ]
      }
    ],
    sidebar: {
      '/install/': [
        { text: 'Install', items: [{ text: 'Installation runbook', link: '/install/agents' }, { text: 'Deployment & operations', link: '/operations/' }, { text: 'Troubleshooting', link: '/support/troubleshooting' }] }
      ],
      '/guide/': [
        { text: 'Start here', items: [{ text: 'Overview', link: '/guide/' }, { text: 'Concepts', link: '/guide/concepts' }, { text: 'Evaluating the gateway', link: '/guide/evaluating' }, { text: 'Installation runbook', link: '/install/agents' }] },
        { text: 'The system', items: [{ text: 'Architecture', link: '/architecture/' }, { text: 'Providers & capabilities', link: '/guide/providers' }, { text: 'Deployment & operations', link: '/operations/' }, { text: 'Security & privacy', link: '/security/' }] },
        { text: 'Keep moving', items: [{ text: 'Troubleshooting', link: '/troubleshooting/' }, { text: 'Releases & upgrades', link: '/troubleshooting/releases' }, { text: 'Client libraries', link: '/clients/' }, { text: 'Getting help', link: '/support/' }, { text: 'Contribute', link: '/contributing/' }] }
      ],
      '/architecture/': [
        {
          text: 'Architecture',
          items: [
            { text: 'Overview', link: '/architecture/' },
            { text: 'Request lifecycle', link: '/architecture/request-lifecycle' },
            { text: 'Accounting', link: '/architecture/accounting' },
            { text: 'Budgets & rate limits', link: '/architecture/budgets-and-rate-limits' },
            { text: 'Providers & adapters', link: '/architecture/providers' },
            { text: 'Catalog & releases', link: '/architecture/catalog' },
            { text: 'Security model', link: '/architecture/security' },
            { text: 'Data & retention', link: '/architecture/data-and-retention' },
            { text: 'Scaling', link: '/architecture/scaling' }
          ]
        }
      ],
      '/quickstarts/': [
        { text: 'Quickstarts', items: [{ text: 'Overview', link: '/quickstarts/' }, { text: 'Installation', link: '/quickstarts/installation' }, { text: 'Authentication', link: '/quickstarts/authentication' }, { text: 'Chat', link: '/quickstarts/chat' }, { text: 'Streaming', link: '/quickstarts/streaming' }, { text: 'Tool calling', link: '/quickstarts/tools' }, { text: 'Embeddings', link: '/quickstarts/embeddings' }, { text: 'Errors', link: '/quickstarts/errors' }] }
      ],
      '/reference/': [{ text: 'API reference', items: [{ text: 'Overview', link: '/reference/' }, { text: 'HTTP endpoints', link: '/reference/endpoints' }, { text: 'Stability and compatibility', link: '/reference/compatibility' }, { text: 'Error codes', link: '/support/errors' }, { text: 'OpenAPI artifact', link: '/openapi/inference-v1.json', target: '_blank' }] }],
      '/operations/': [{ text: 'Operations', items: [{ text: 'Deployment', link: '/operations/' }, { text: 'Reliability signals', link: '/operations/signals' }, { text: 'Budgets and their limits', link: '/operations/budgets' }, { text: 'Backup and recovery', link: '/operations/recovery' }, { text: 'Rollback and incidents', link: '/operations/incidents' }] }],
      '/security/': [{ text: 'Security & privacy', items: [{ text: 'Boundaries and reporting', link: '/security/' }, { text: 'Security model', link: '/architecture/security' }] }],
      '/support/': [{ text: 'Support', items: [{ text: 'Getting help', link: '/support/' }, { text: 'Troubleshooting guide', link: '/support/troubleshooting' }, { text: 'Error codes', link: '/support/errors' }] }],
      '/troubleshooting/': [{ text: 'Troubleshooting', items: [{ text: 'Common problems', link: '/troubleshooting/' }, { text: 'Troubleshooting guide', link: '/support/troubleshooting' }, { text: 'Releases & upgrades', link: '/troubleshooting/releases' }] }],
      '/clients/': [{ text: 'Client libraries', items: [{ text: 'Overview', link: '/clients/' }] }],
      '/contributing/': [{ text: 'Contributing', items: [{ text: 'Rules', link: '/contributing/' }] }]
    },
    socialLinks: [{ icon: 'github', link: REPOSITORY, ariaLabel: 'QuCelerate AI Gateway on GitHub' }],
    // An empty aria-label leaves the button named by its visible text and
    // shortcut, as WCAG 2.5.3 (label in name) asks; the default "Search"
    // omits the shortcut it shows.
    search: { provider: 'local', options: { translations: { button: { buttonAriaLabel: '' } } } },
    outline: { level: [2, 3] },
    footer: { message: 'Open source under the MIT license • No tenant data or credentials', copyright: 'QuCelerate AI Gateway contributors' }
  }
})
