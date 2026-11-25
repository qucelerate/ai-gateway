// The documentation release gate's accessibility check (plan §9: WCAG 2.2
// AA): every page of the built site, in light and dark mode, scanned with
// axe for WCAG 2.0–2.2 A and AA rules. Serves .vitepress/dist itself, so run
// `npm run build` first. Run from apps/docs:
//
//   node scripts/check-accessibility.mjs
//
// Needs Chromium for Playwright (npx playwright install chromium).

import { createReadStream, existsSync, statSync } from 'node:fs';
import { createServer } from 'node:http';
import { extname, join, normalize } from 'node:path';
import { execFileSync } from 'node:child_process';
import { chromium } from 'playwright';
import { AxeBuilder } from '@axe-core/playwright';

const ROOT = new URL('..', import.meta.url).pathname;
const DIST = join(ROOT, '.vitepress/dist');
if (!existsSync(join(DIST, 'index.html'))) {
  console.error('no built site in .vitepress/dist; run npm run build first');
  process.exit(1);
}

const TYPES = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.json': 'application/json', '.svg': 'image/svg+xml', '.woff2': 'font/woff2' };
// The built site as a static host with clean URLs serves it.
const server = createServer((request, response) => {
  const path = normalize(decodeURIComponent(new URL(request.url, 'http://x').pathname)).replace(/^(\.\.[/\\])+/, '');
  const candidates = [join(DIST, path), join(DIST, `${path}.html`), join(DIST, path, 'index.html')];
  const file = candidates.find(candidate => candidate.startsWith(DIST) && existsSync(candidate) && statSync(candidate).isFile());
  if (!file) {
    response.writeHead(404).end();
    return;
  }
  response.writeHead(200, { 'content-type': TYPES[extname(file)] ?? 'application/octet-stream' });
  createReadStream(file).pipe(response);
});
await new Promise(ready => server.listen(0, '127.0.0.1', ready));
const base = `http://127.0.0.1:${server.address().port}`;

// Every page, from its source.
const pages = execFileSync('git', ['ls-files', '*.md'], { cwd: ROOT, encoding: 'utf8' })
  .split('\n')
  .filter(file => file && file !== 'README.md')
  .map(file => `/${file.replace(/(^|\/)index\.md$/, '$1').replace(/\.md$/, '')}`);

const browser = await chromium.launch();
const failures = [];
try {
  for (const scheme of ['light', 'dark']) {
    const context = await browser.newContext({ colorScheme: scheme });
    // VitePress follows the stored preference over the system's.
    await context.addInitScript(value => localStorage.setItem('vitepress-theme-appearance', value), scheme);
    const page = await context.newPage();
    for (const path of pages) {
      const response = await page.goto(`${base}${path}`);
      if (!response?.ok()) {
        failures.push(`${path} (${scheme}): HTTP ${response?.status()}`);
        continue;
      }
      const { violations } = await new AxeBuilder({ page })
        .withTags(['wcag2a', 'wcag2aa', 'wcag21a', 'wcag21aa', 'wcag22aa'])
        .analyze();
      for (const violation of violations) {
        const targets = violation.nodes.slice(0, 3).map(node => node.target.join(' ')).join('; ');
        failures.push(`${path} (${scheme}): ${violation.id} [${violation.impact}] ${violation.help} — ${violation.nodes.length} element(s): ${targets}`);
      }
    }
    await context.close();
  }
} finally {
  await browser.close();
  server.close();
}

if (failures.length) {
  console.error(`${failures.length} accessibility violation(s):`);
  for (const failure of failures) console.error(`  ${failure}`);
  process.exit(1);
}
console.log(`accessibility checks passed: ${pages.length} pages, light and dark, WCAG 2.2 A/AA rules`);
