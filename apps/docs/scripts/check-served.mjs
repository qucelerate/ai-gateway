// Smoke-tests the docs image as served, with its real nginx rules and
// response headers (check:a11y serves the files itself, without them):
// clean URLs, the OpenAPI's content type, the 404 page, the security
// headers, and pages and local search working under the CSP. Run from
// apps/docs against a running container:
//
//   node scripts/check-served.mjs http://127.0.0.1:8080
//
// Prints each failure and exits non-zero if any.

import { chromium } from 'playwright';

const base = (process.argv[2] ?? '').replace(/\/$/, '');
if (!base) {
  console.error('usage: check-served.mjs BASE_URL');
  process.exit(2);
}
const failures = [];
const expect = (ok, message) => ok || failures.push(message);

const routes = [
  ['/', 200, 'text/html'],
  ['/guide/concepts', 200, 'text/html'],
  ['/guide/', 200, 'text/html'],
  ['/openapi/inference-v1.json', 200, 'application/json'],
  ['/no-such-page', 404, 'text/html'],
];
for (const [path, status, type] of routes) {
  const response = await fetch(base + path, { redirect: 'manual' });
  expect(response.status === status, `${path}: status ${response.status}, expected ${status}`);
  expect((response.headers.get('content-type') ?? '').startsWith(type), `${path}: content type ${response.headers.get('content-type')}, expected ${type}`);
  const csp = response.headers.get('content-security-policy') ?? '';
  expect(csp.includes("default-src 'self'") && csp.includes("frame-ancestors 'none'"), `${path}: missing or weak Content-Security-Policy`);
  expect(response.headers.get('x-content-type-options') === 'nosniff', `${path}: missing X-Content-Type-Options`);
}
const notFound = await (await fetch(`${base}/no-such-page`)).text();
expect(notFound.includes('<div id="app">'), '/no-such-page: not the site\'s 404 page');

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  const errors = [];
  page.on('console', message => message.type() === 'error' && errors.push(message.text()));
  page.on('pageerror', error => errors.push(String(error)));
  page.on('response', response => response.status() >= 400 && errors.push(`${response.status()} ${response.url()}`));
  for (const path of ['/', '/guide/concepts', '/reference/']) await page.goto(base + path, { waitUntil: 'networkidle' });
  await page.goto(`${base}/guide/`, { waitUntil: 'networkidle' });
  await page.locator('#local-search button').first().click();
  await page.waitForSelector('.VPLocalSearchBox input', { timeout: 10000 });
  await page.fill('.VPLocalSearchBox input', 'streaming');
  await page.waitForSelector('.VPLocalSearchBox .result', { timeout: 10000 }).catch(() => {});
  expect((await page.locator('.VPLocalSearchBox .result').count()) > 0, 'search for "streaming" found nothing');
  expect(errors.length === 0, `browser errors: ${errors.join('; ')}`);
} finally {
  await browser.close();
}

if (failures.length) {
  for (const failure of failures) console.error(failure);
  process.exit(1);
}
console.log(`served-site checks passed against ${base}`);
