import { expect, test } from '@playwright/test';
import { seedOwner, signIn } from '../support/seed.js';
import { openSection, selectOrganization } from '../support/shell.js';

// An organization owner invites a teammate as a member from the Organization
// screen: the link is shown once to send on, the invitation waits in the
// list, and revoking it stops the link from working.

test('an owner invites a member, sees the link once, and revokes it', async ({ page, context, baseURL, request }) => {
  const owner = await seedOwner('invite-member');
  await signIn(context, owner, baseURL!);

  await test.step('open the invitations of the organization', async () => {
    await page.goto('/admin/organization');
    await selectOrganization(page, owner.organizationName);
    await openSection(page, 'People', 'Invitations');
    await expect(page.getByText('No invitations')).toBeVisible();
  });

  let link = '';
  await test.step('invite someone as a member', async () => {
    await page.getByRole('button', { name: 'Invite', exact: true }).click();
    const dialog = page.getByRole('dialog', { name: 'Invite someone' });
    await dialog.getByLabel('Role').selectOption('member');
    await dialog.getByLabel('Link works for').selectOption({ label: '3 days' });
    await dialog.getByRole('button', { name: 'Create link' }).click();
    link = (await page.getByLabel('Invitation link', { exact: true }).textContent())?.trim() ?? '';
    expect(link).toMatch(new RegExp(`^${baseURL}/api/admin/v1/invitations/accept\\?token=[A-Za-z0-9_-]{43}$`));
    await expect(page.getByText('send it to the person you are inviting')).toBeVisible();
    await expect(page.getByRole('button', { name: 'Copy invitation link' })).toBeVisible();
  });

  await test.step('the link is shown only once', async () => {
    await page.getByRole('button', { name: 'Done' }).click();
    await expect(page.getByLabel('Invitation link', { exact: true })).toHaveCount(0);
    await page.reload();
    await selectOrganization(page, owner.organizationName);
    await openSection(page, 'People', 'Invitations');
    await expect(page.getByText(link)).toHaveCount(0);
  });

  const table = page.getByRole('table', { name: 'Invitations' });
  await test.step('it waits in the list as pending', async () => {
    const row = table.getByRole('row').filter({ hasText: 'Member' });
    await expect(row).toHaveCount(1);
    await expect(row).toContainText('pending');
    // The link is good until it is revoked. With local accounts on, it
    // opens the admin UI's invitation page (password or provider), the
    // token moved into the fragment.
    const token = new URL(link).searchParams.get('token');
    const page_ = await request.get(link, { maxRedirects: 0 });
    expect(page_.status()).toBe(303);
    expect(page_.headers()['location']).toBe(`/admin/accept-invitation#token=${token}`);
    // Through the provider, it gets past the invitation check to start a
    // sign-in, which in this stack (no identity provider) stops at the
    // provider instead of redirecting to it.
    const start = await request.get(`${link}&method=sso`, { maxRedirects: 0 });
    expect(start.status()).not.toBe(404);
    expect(await start.json()).toEqual({ error: 'oidc_provider_unavailable' });
  });

  await test.step('revoke it', async () => {
    await table.getByRole('button', { name: 'Revoke' }).click();
    await page.getByRole('alertdialog', { name: 'Revoke invitation' }).getByRole('button', { name: 'Revoke' }).click();
    await expect(table.getByRole('row').filter({ hasText: 'Member' })).toContainText('revoked');
    await expect(table.getByRole('button', { name: 'Revoke' })).toHaveCount(0);
    // The link no longer works.
    const spent = await request.get(link, { maxRedirects: 0 });
    expect(spent.status()).toBe(404);
    expect(await spent.json()).toEqual({ error: 'invitation_invalid' });
  });
});
