import { randomUUID } from 'node:crypto';
import { expect, test, type APIRequestContext, type Page } from '@playwright/test';
import { setChecked } from './carbon.js';
import { CONNECT_TAB, openManaged, openSection } from './shell.js';

// The steps every live spec shares once a cloud's own setup is done: verify
// the connection, offer a model, save the connection, have a budget, and call
// the model with a new API key. Each keeps what it sets up, so a rerun finds
// it and goes on.

// A LIVE_* setting. Without a fallback the setting is required: the account,
// tenant, resource and project of a live run are the operator's own and have
// no default (live.sh reads them from .env.live, which git ignores).
export const env = (name: string, fallback?: string): string => {
  const value = process.env[name] || fallback;
  if (value === undefined) throw new Error(`${name} must be set for a live run (see tests/functional/README.md)`);
  return value;
};

/** USD per million tokens. */
export type Prices = { input: string; output: string; cachedInput: string };

/** Verifies the connection until it is connected, retrying while a new grant
 * propagates. `onAwaiting` runs once if the operator has yet to bind the
 * cloud principal. Returns the last result's text. */
export async function verify(page: Page, onAwaiting?: () => Promise<void> | void): Promise<string> {
  const result = page.getByRole('status').filter({ hasText: /^(Connected|Not connected|Not available|Awaiting)/ });
  let approved = !onAwaiting;
  for (let attempt = 1; ; attempt++) {
    await page.getByRole('button', { name: 'Verify connection' }).click();
    await expect(page.getByRole('button', { name: 'Verify connection' })).toBeEnabled();
    await expect(result).toBeVisible();
    const text = (await result.textContent()) ?? '';
    if (text.startsWith('Awaiting') && !approved) {
      approved = true;
      await onAwaiting!();
      continue;
    }
    if (text.startsWith('Connected.') || attempt === 6) return text;
    await page.waitForTimeout(10_000);
  }
}

/** Adds the account to the catalog with `model` under `alias`, unless it is
 * already there, then saves the connection for it. */
export async function offerAndSave(page: Page, accountKey: string, model: string, alias: string, prices: Prices) {
  await test.step('add the model under its alias, unless the account is already offered', async () => {
    const add = page.getByRole('region', { name: 'Add it as a provider' });
    const save = page.getByRole('button', { name: `Save for ${accountKey}` });
    await expect(add.or(save).first()).toBeVisible();
    if (!(await add.isVisible())) {
      console.log(`${accountKey} is already in the catalog; not adding it again`);
      return;
    }
    const filter = add.getByLabel('Find a model');
    if (await filter.isVisible()) await filter.fill(model);
    await setChecked(add.getByRole('checkbox', { name: model, exact: true }), true);
    const group = add.getByRole('group', { name: model });
    await group.getByLabel('Alias').fill(alias);
    await group.getByLabel(/^Input/).fill(prices.input);
    await group.getByLabel(/^Output/).fill(prices.output);
    const cached = group.getByLabel(/^Cached input/);
    if (await cached.isVisible()) await cached.fill(prices.cachedInput);
    await add.getByRole('button', { name: `Add ${accountKey} and activate` }).click();
    await expect(save).toBeVisible();
  });

  await test.step('save the connection for the account', async () => {
    await page.getByRole('button', { name: `Save for ${accountKey}` }).click();
    await expect(page.getByText(`Saved for ${accountKey}.`)).toBeVisible();
  });
}

/** An organization budget, which every request is charged against. */
export async function ensureBudget(page: Page) {
  await test.step('have a budget, which every request is charged against', async () => {
    await openManaged(page, 'Policies');
    const budgets = page.getByRole('table', { name: 'Budgets' });
    const none = page.getByRole('alert').filter({ hasText: 'No budget is set' });
    await expect(budgets.or(none).first()).toBeVisible();
    if (await budgets.isVisible() && (await budgets.textContent())?.includes('Whole organization')) return;
    await page.getByRole('button', { name: 'Add budget' }).click();
    const dialog = page.getByRole('dialog', { name: 'Add budget' });
    await dialog.getByLabel('Limit').fill(env('LIVE_BUDGET', '25.00'));
    await dialog.getByRole('button', { name: 'Save' }).click();
    await expect(budgets).toContainText('Whole organization');
  });
}

/** Creates a service account and key, calls `alias` with it, and then
 * retires the account (and any a failed run left). */
export async function callWithNewKey(page: Page, request: APIRequestContext, alias: string) {
  const account = `functional-${new Date().toISOString().slice(0, 19).replace(/[-:T]/g, '')}`;
  let secret = '';
  try {
    await test.step('create a service account and an API key', async () => {
      await openManaged(page, 'Identities');
      await page.getByRole('button', { name: 'Create service account' }).click();
      await page.getByLabel('Account name').fill(account);
      await page.getByRole('dialog').getByRole('button', { name: 'Create', exact: true }).click();
      await expect(page.getByRole('dialog')).toBeHidden();
      // A new account opens on its own page.
      await expect(page.getByRole('heading', { name: account, level: 3 })).toBeVisible();
      await page.getByRole('button', { name: 'Create key' }).click();
      await page.getByRole('dialog', { name: 'Create API key' }).getByRole('button', { name: /Create/ }).click();
      secret = (await page.getByLabel('API key secret').textContent())?.trim() ?? '';
      expect(secret).not.toBe('');
    });

    await test.step('call the model through its alias', async () => {
      const response = await request.post('/api/inference/v1/chat/completions', {
        headers: { authorization: `Bearer ${secret}`, 'idempotency-key': randomUUID() },
        data: { model: alias, messages: [{ role: 'user', content: 'Reply with the single word: pong' }], max_tokens: 16 },
      });
      expect(response.status(), await response.text()).toBe(200);
      const body = await response.json();
      console.log(`${alias} answered: ${JSON.stringify(body.choices[0].message.content)} (${body.usage.input_tokens} in, ${body.usage.output_tokens} out)`);
      expect(body.choices[0].message.content).not.toBe('');
      expect(body.usage.input_tokens).toBeGreaterThan(0);
    });
  } finally {
    await test.step('revoke the key and disable the service account, and any a failed run left', async () => {
      await page.getByRole('button', { name: 'I have saved it' }).click().catch(() => undefined);
      await retireServiceAccounts(page, /^functional-\d{14}$/);
    });
  }
}

/** The enabled service accounts named like `search`, every one loaded (the
 * list shows enabled accounts unless Show disabled is on). */
async function enabledServiceAccounts(page: Page, search: string) {
  const back = page.getByRole('link', { name: '← Service accounts' });
  if (await back.isVisible()) await back.click();
  await expect(page.getByRole('status').filter({ hasText: 'Loading service accounts' })).toBeHidden();
  const table = page.getByRole('table', { name: 'Service accounts' });
  const none = page.getByRole('heading', { name: 'No active service accounts' });
  await expect(table.or(none).first()).toBeVisible();
  if (await none.isVisible()) return table;
  const more = page.getByRole('button', { name: 'Load more' });
  while (await more.isVisible()) {
    const shown = await table.getByRole('row').count();
    await more.click();
    await expect(table.getByRole('row')).not.toHaveCount(shown);
  }
  await page.getByRole('searchbox', { name: 'Search service accounts' }).fill(search);
  return table;
}

/** Retires the active service accounts named like `pattern`: revokes their
 * keys and disables them. One that has made requests cannot be deleted, as
 * its usage is kept. */
async function retireServiceAccounts(page: Page, pattern: RegExp) {
  // By name: the list reloads after every change.
  const table = await enabledServiceAccounts(page, 'functional-');
  const names = (await table.getByRole('link').allTextContents()).filter(name => pattern.test(name));
  for (const name of names) {
    await (await enabledServiceAccounts(page, name)).getByRole('link', { name, exact: true }).click();
    await expect(page.getByRole('heading', { name, level: 3 })).toBeVisible();
    // Its usable keys, once they have loaded.
    const keys = page.getByRole('table', { name: 'API keys' });
    await expect(keys.or(page.getByRole('heading', { name: 'No usable API keys' })).first()).toBeVisible();
    // A key's status badge reads `active`; the header row names the column `Status`.
    const active = keys.getByRole('row').filter({ has: page.getByRole('cell').getByText('active', { exact: true }) });
    while (await active.count() > 0) {
      await active.first().getByRole('button', { name: 'Revoke' }).click();
      const dialog = page.getByRole('alertdialog', { name: 'Revoke API key' });
      await dialog.getByRole('button', { name: 'Revoke' }).click();
      await expect(dialog).toBeHidden();
    }
    await page.getByRole('button', { name: 'Disable', exact: true }).click();
    const dialog = page.getByRole('alertdialog', { name: 'Disable service account' });
    await dialog.getByRole('button', { name: 'Disable' }).click();
    await expect(dialog).toBeHidden();
    await expect(page.getByRole('heading', { name, level: 3 }).locator('..')).toContainText('disabled');
  }
}

/** Opens Connect a provider for `cloud`. */
export async function connectCloud(page: Page, cloud: 'Amazon Web Services' | 'Microsoft Azure' | 'Google Cloud') {
  await openSection(page, 'Connections', CONNECT_TAB);
  await page.getByRole('radio', { name: new RegExp(cloud) }).click();
}
