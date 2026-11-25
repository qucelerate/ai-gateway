// node --test scripts/llms.test.mjs
import assert from 'node:assert/strict';
import { test } from 'node:test';
import { absoluteLinks } from './llms.mjs';

const SITE = 'https://gateway.qucelerate.com';

test('relative links resolve against the page directory', () => {
  assert.equal(absoluteLinks('[q](../quickstarts/errors.md#codes)', 'support/errors.md'), `[q](${SITE}/quickstarts/errors.md#codes)`);
  assert.equal(absoluteLinks('[s](sibling.md)', 'guide/a.md'), `[s](${SITE}/guide/sibling.md)`);
  assert.equal(absoluteLinks('[r](/operations/index.md#profiles)', 'guide/a.md'), `[r](${SITE}/operations/index.md#profiles)`);
  assert.equal(absoluteLinks('[h](#here)', 'guide/a.md'), `[h](${SITE}/guide/a.md#here)`);
  assert.equal(absoluteLinks('[ref]: ../x.md', 'guide/a.md'), `[ref]: ${SITE}/x.md`);
});

test('absolute links, link titles and fenced code are kept', () => {
  assert.equal(absoluteLinks('[w](https://example.com/a) [m](mailto:a@b.c)', 'a.md'), '[w](https://example.com/a) [m](mailto:a@b.c)');
  assert.equal(absoluteLinks('[t](b.md "Title")', 'g/a.md'), `[t](${SITE}/g/b.md "Title")`);
  const fenced = '```\n[x](../y.md)\n```';
  assert.equal(absoluteLinks(fenced, 'g/a.md'), fenced);
});
