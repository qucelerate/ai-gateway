import { expect, test, type Page } from '@playwright/test';
import { fakeRequests } from './fakes.js';
import type { Owner } from './seed.js';
import { setChecked } from './carbon.js';
import { CONNECT_TAB, openManaged, openSection, selectOrganization } from './shell.js';

// What a new tenant does before its first inference call, through the admin
// UI: connect its AWS account, offer one of its Bedrock models under an
// alias, save the connection, set a budget and create an API key. The specs
// that call the model share it (connect-aws, clients).

export const ACCOUNT = '123456789012';
export const MODEL = 'anthropic.claude-3-5-haiku-20241022-v1:0';
export const ALIAS = 'haiku';

/** Provisions `owner`'s organization on `page` (signed in) and returns the API key secret. */
export async function provisionAwsTenant(page: Page, owner: Owner, accountName = 'functional-client'): Promise<string> {
  await test.step('show the instructions for the account', async () => {
    // The old address: newer images redirect it to Connections.
    await page.goto('/admin/providers/connect');
    await selectOrganization(page, owner.organizationName);
    await openSection(page, 'Connections', CONNECT_TAB);
    await page.getByRole('radio', { name: /Amazon Web Services/ }).click();
    await page.getByLabel('Account ID').fill(ACCOUNT);
    await page.getByLabel('Region').fill('us-east-2');
    await page.getByRole('button', { name: 'Show instructions' }).click();
    await expect(page.getByRole('link', { name: 'Launch in AWS CloudFormation' })).toBeVisible();
  });

  const externalId = `qucelerate-${owner.organizationId}`;
  await test.step('verify the connection', async () => {
    await expect(page.getByText(externalId, { exact: true })).toBeVisible();
    await expect(page.getByRole('link', { name: 'Launch in AWS CloudFormation' })).toHaveAttribute('href', new RegExp(`param_ExternalId=${externalId}`));
    await page.getByRole('button', { name: 'Verify connection' }).click();
    await expect(page.getByRole('status').filter({ hasText: 'Connected.' })).toBeVisible();
    // The gateway assumed the tenant's role, for this organization only.
    const sts = (await fakeRequests()).filter(item => item.path === '/sts/');
    expect(sts.at(-1)).toMatchObject({ roleArn: `arn:aws:iam::${ACCOUNT}:role/QucelerateGateway`, externalId });
  });

  await test.step('add a model under an alias, with its prices', async () => {
    const add = page.getByRole('region', { name: 'Add it as a provider' });
    await setChecked(add.getByRole('checkbox', { name: MODEL, exact: true }), true);
    const model = add.getByRole('group', { name: MODEL });
    await model.getByLabel('Alias').fill(ALIAS);
    await model.getByLabel(/^Input/).fill('0.80');
    await model.getByLabel(/^Output/).fill('4.00');
    await add.getByRole('button', { name: `Add bedrock-${ACCOUNT} and activate` }).click();
  });

  await test.step('save the connection for the account', async () => {
    await page.getByRole('button', { name: `Save for bedrock-${ACCOUNT}` }).click();
    await expect(page.getByText(`Saved for bedrock-${ACCOUNT}.`)).toBeVisible();
  });

  await test.step('set a monthly budget, which requests are charged against', async () => {
    await openManaged(page, 'Policies');
    await page.getByRole('button', { name: 'Add budget' }).click();
    const dialog = page.getByRole('dialog', { name: 'Add budget' });
    await dialog.getByLabel('Limit').fill('25.00');
    await dialog.getByRole('button', { name: 'Save' }).click();
    await expect(page.getByRole('table', { name: 'Budgets' })).toContainText('Whole organization');
  });

  let secret = '';
  await test.step('create an API key', async () => {
    await openManaged(page, 'Identities');
    await page.getByRole('button', { name: /^Create( service account)?$/ }).first().click();
    await page.getByLabel('Account name').fill(accountName);
    await page.getByRole('dialog').getByRole('button', { name: 'Create', exact: true }).click();
    // Newer images open the new account's page; older ones list it to pick.
    const opened = page.getByRole('heading', { name: accountName, level: 3 });
    const option = page.getByRole('option', { name: new RegExp(accountName) });
    await expect(opened.or(option).first()).toBeVisible();
    if (await option.isVisible()) await option.click();
    await page.getByRole('button', { name: 'Create key' }).click();
    await page.getByRole('dialog').getByRole('button', { name: /Create/ }).click();
    secret = (await page.getByLabel('API key secret').textContent())?.trim() ?? '';
    expect(secret).not.toBe('');
  });
  return secret;
}
