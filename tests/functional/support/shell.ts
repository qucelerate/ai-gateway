import { expect, type Page } from '@playwright/test';

/**
 * The console's shell: the organization dropdown in the header (as in IBM
 * Cloud) and the side navigation. An older image, as the published one the
 * client checks run against may be, chose the organization from a select in
 * the side navigation; both are handled.
 */

const legacySelect = (page: Page) => page.getByRole('combobox', { name: 'Select organization' });
const dropdown = (page: Page) => page.getByRole('button', { name: /^(Select organization|Organization: .*\. Switch organization)$/ });

/** Waits for the shell, then says whether it is the older one. */
async function legacy(page: Page): Promise<boolean> {
  await expect(legacySelect(page).or(dropdown(page)).first()).toBeVisible();
  return legacySelect(page).isVisible();
}

/** The organizations the person can open. */
export async function organizationNames(page: Page): Promise<string[]> {
  if (await legacy(page)) {
    return (await legacySelect(page).locator('option').allTextContents()).filter(option => !option.endsWith('…'));
  }
  await dropdown(page).click();
  const names = await page.getByRole('menuitemradio').allTextContents();
  await page.keyboard.press('Escape');
  return names;
}

/** Opens organization `name` and waits until it is confirmed. */
export async function selectOrganization(page: Page, name: string): Promise<void> {
  if (await legacy(page)) {
    await legacySelect(page).selectOption({ label: name });
    return;
  }
  const button = dropdown(page);
  if ((await button.getAttribute('aria-label')) === `Organization: ${name}. Switch organization`) return;
  await button.click();
  await page.getByRole('menuitemradio', { name, exact: true }).click();
  await expect(page.getByRole('button', { name: `Organization: ${name}. Switch organization` })).toBeVisible();
  await expect(page.getByRole('button', { name: `Organization: ${name}. Switch organization` })).not.toHaveAttribute('aria-busy', 'true');
}

/** The open organization's ID, as the console remembers it. */
export async function organizationId(page: Page): Promise<string> {
  if (await legacy(page)) return legacySelect(page).inputValue();
  return page.evaluate(() => localStorage.getItem('gateway-admin.organization') ?? '');
}

/** The Connections tab that connects an account: "Connect a provider" since
 * Anthropic and OpenAI keys joined it, "Connect a cloud" before. The client
 * compatibility runs these helpers against published consoles of either age. */
export const CONNECT_TAB = /^Connect a (provider|cloud)$/;

/**
 * Opens `section` of `group` in the side navigation (its first section when
 * none is named), or a page without sections. Pages with sections open as an
 * accordion, as in IBM Cloud. Older images, as the published ones the client
 * checks run against may be, linked each page and showed its sections as
 * tabs; the release before kept the administering pages under Manage in the
 * header.
 */
export async function openSection(page: Page, group: string, section?: string | RegExp): Promise<void> {
  const nav = page.getByRole('navigation');
  // Its accessible name leaves out the icon, unlike its text.
  const parent = nav.getByRole('button', { name: group, exact: true }).and(nav.locator('[aria-expanded]'));
  const link = nav.getByRole('link', { name: group, exact: true });
  const manage = page.getByRole('button', { name: 'Manage', exact: true });
  await expect(parent.or(link).or(manage).first()).toBeVisible();
  if (await parent.count()) {
    if ((await parent.getAttribute('aria-expanded')) !== 'true') await parent.click();
    const list = nav.locator(`#${await parent.getAttribute('aria-controls')}`);
    await (section ? list.getByRole('link', { name: section, exact: true }) : list.getByRole('link').first()).click();
    return;
  }
  if (await link.isVisible()) {
    await link.click();
  } else {
    await manage.click();
    await page.getByRole('menu', { name: 'Manage' }).getByRole('menuitem', { name: group, exact: true }).click();
  }
  if (section) await page.getByRole('tab', { name: section }).click();
}

/** Goes to a page of the side navigation (Settings, Policies, People,
 * Identities, Audit, Organizations among them), at its first section. */
export async function openManaged(page: Page, label: string): Promise<void> {
  await openSection(page, label);
}
