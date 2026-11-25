import { expect, test } from '@playwright/test';
import { seedOwner, signIn } from '../support/seed.js';
import { setChecked } from '../support/carbon.js';
import { openSection, selectOrganization } from '../support/shell.js';

// An organization owner adds a webhook endpoint, keeps its signing secret
// (shown once), disables the endpoint, looks at its deliveries and deletes
// it, through the Policies screen.

const URL = 'https://hooks.example.com/gateway';

test('an owner manages a webhook endpoint and its one-time secret', async ({ page, context, baseURL }) => {
  const owner = await seedOwner('webhooks');
  await signIn(context, owner, baseURL!);
  await page.goto('/admin/policies');
  await selectOrganization(page, owner.organizationName);
  await openSection(page, 'Policies', 'Webhooks');
  await expect(page.getByRole('status').filter({ hasText: 'No webhook endpoints' })).toBeVisible();

  await test.step('add an endpoint and see its secret once', async () => {
    await page.getByRole('button', { name: 'Add endpoint' }).click();
    const dialog = page.getByRole('dialog', { name: 'Add webhook endpoint' });
    await dialog.getByLabel('URL').fill(URL);
    await setChecked(dialog.getByRole('checkbox', { name: /reconciliation\.updated/ }), true);
    await dialog.getByRole('button', { name: 'Add endpoint' }).click();
    const secret = page.getByLabel('Webhook signing secret');
    await expect(secret).toHaveText(/^whsec_/);
    await page.getByRole('button', { name: 'I have saved it' }).click();
    await expect(page.getByLabel('Webhook signing secret')).toHaveCount(0);
  });

  const table = page.getByRole('table', { name: 'Webhook endpoints' });
  await test.step('disable it', async () => {
    await expect(table).toContainText(URL);
    await expect(table).toContainText('reconciliation.updated');
    await page.getByRole('button', { name: `Edit ${URL}` }).click();
    const dialog = page.getByRole('dialog', { name: 'Edit webhook endpoint' });
    await setChecked(dialog.getByRole('checkbox', { name: /^Enabled/ }), false);
    await dialog.getByRole('button', { name: 'Save' }).click();
    await expect(page.getByText('Endpoint saved.')).toBeVisible();
    await expect(table).toContainText('Disabled');
  });

  await test.step('it has no deliveries yet', async () => {
    await page.getByRole('button', { name: `Deliveries to ${URL}` }).click();
    await expect(page.getByText('No deliveries yet')).toBeVisible();
  });

  await test.step('delete it after confirming', async () => {
    await page.getByRole('button', { name: `Delete ${URL}` }).click();
    await page.getByRole('alertdialog', { name: 'Delete webhook endpoint?' }).getByRole('button', { name: 'Delete endpoint' }).click();
    await expect(page.getByText('Endpoint deleted, with all of its deliveries.')).toBeVisible();
    await expect(page.getByRole('status').filter({ hasText: 'No webhook endpoints' })).toBeVisible();
  });
});
