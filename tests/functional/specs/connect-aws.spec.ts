import { randomUUID } from 'node:crypto';
import { expect, test } from '@playwright/test';
import { ALIAS, MODEL, provisionAwsTenant } from '../support/aws-tenant.js';
import { fakeRequests, resetFakes } from '../support/fakes.js';
import { seedOwner, signIn } from '../support/seed.js';
import { openManaged, openSection } from '../support/shell.js';

// An organization owner connects their AWS account, offers one of its
// Bedrock models under an alias, saves the connection and calls the model
// with an API key: the path a new tenant takes, end to end through the
// admin UI, the gateway and (faked) AWS.

test('an owner connects AWS, adds a Bedrock model and calls it', async ({ page, context, baseURL, request }) => {
  await resetFakes();
  const owner = await seedOwner('connect-aws');
  await signIn(context, owner, baseURL!);
  const secret = await provisionAwsTenant(page, owner);

  await test.step('call the model through its alias', async () => {
    const response = await request.post('/api/inference/v1/chat/completions', {
      headers: { authorization: `Bearer ${secret}`, 'idempotency-key': randomUUID() },
      data: { model: ALIAS, messages: [{ role: 'user', content: 'ping' }], max_tokens: 64 },
    });
    expect(response.status(), await response.text()).toBe(200);
    const body = await response.json();
    expect(body.choices[0].message.content).toBe(`fake ${MODEL} heard: ping`);
    expect(body.usage).toMatchObject({ input_tokens: 12, output_tokens: 7, quality: 'exact' });
    // Signed with the role's credentials, not the gateway's own.
    const converse = (await fakeRequests()).filter(item => item.model);
    expect(converse.at(-1)).toMatchObject({ model: MODEL, signedWith: 'ASIAFUNCTIONALTEST01' });
  });

  await test.step('see the request charged at the prices entered', async () => {
    await openSection(page, 'Usage', 'Dashboard');
    const main = page.getByRole('main');
    // The dashboard lists the request, linking to its trace.
    const requests = main.getByRole('table', { name: 'Requests in this view' });
    await expect(requests).toContainText('$0.0000376');
    await expect(requests.getByRole('link', { name: /Open the trace/ })).toHaveCount(1);
    // The headline figures: one request, its tokens and what it was charged,
    // 12 × $0.80 and 7 × $4.00 per million tokens.
    const tile = (name: string | RegExp) => main.locator('article.stat-tile').filter({ has: page.locator('.card-top', { hasText: name }) });
    await expect(tile(/^Requests$/).locator('.report-figure')).toHaveText('1');
    await expect(tile('Input tokens').locator('.report-figure')).toHaveText('12');
    await expect(tile('Output tokens').locator('.report-figure')).toHaveText('7');
    await expect(tile('Spend · USD').locator('.report-figure')).toHaveText('$0.0000376');
  });

  await test.step('revoke the key and disable the service account', async () => {
    await openManaged(page, 'Identities');
    await page.getByRole('table', { name: 'Service accounts' }).getByRole('link', { name: 'functional-client', exact: true }).click();
    await page.getByRole('table', { name: 'API keys' }).getByRole('button', { name: 'Revoke' }).click();
    await page.getByRole('alertdialog', { name: 'Revoke API key' }).getByRole('button', { name: 'Revoke' }).click();
    await expect(page.getByRole('alertdialog', { name: 'Revoke API key' })).toBeHidden();
    const refused = await request.post('/api/inference/v1/chat/completions', {
      headers: { authorization: `Bearer ${secret}`, 'idempotency-key': randomUUID() },
      data: { model: ALIAS, messages: [{ role: 'user', content: 'ping' }], max_tokens: 64 },
    });
    expect(refused.status()).toBe(401);
    await page.getByRole('button', { name: 'Disable', exact: true }).click();
    await page.getByRole('alertdialog', { name: 'Disable service account' }).getByRole('button', { name: 'Disable' }).click();
    await expect(page.locator('.detail-header')).toContainText('disabled');
    // Disabled accounts leave the list unless asked for.
    await page.getByRole('link', { name: '← Service accounts' }).click();
    await expect(page.getByRole('heading', { name: 'No active service accounts' })).toBeVisible();
  });
});
