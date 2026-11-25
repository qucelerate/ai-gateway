import { chmodSync, existsSync, mkdirSync } from 'node:fs';
import { dirname } from 'node:path';
import { expect, type Browser, type BrowserContext, type Page } from '@playwright/test';
import { organizationId, organizationNames, selectOrganization } from './shell.js';

// Real sign-in for the live project: the first run opens the gateway's
// sign-in, and a person completes it with the identity provider (MFA
// included); the session is then kept in .auth/live.json, readable only by
// its owner and ignored by git, until it expires (OIDC_SESSION_TTL_SECONDS).

const STATE = new URL('../.auth/live.json', import.meta.url).pathname;
const SIGN_IN_WAIT = 10 * 60_000;

export async function signedIn(browser: Browser, baseURL: string): Promise<{ context: BrowserContext; page: Page }> {
  const context = await browser.newContext({ baseURL, storageState: existsSync(STATE) ? STATE : undefined });
  const page = await context.newPage();
  await page.goto('/admin/overview');
  const signOut = page.getByRole('button', { name: 'Sign out' });
  // Older gateways title the page Sign-in required.
  const required = page.getByRole('heading', { name: /^(Sign in to Gateway|Sign-in required)$/ });
  await expect(signOut.or(required)).toBeVisible();
  if (await required.isVisible()) {
    console.log(`Sign in to ${baseURL} in the browser window (waiting up to ${SIGN_IN_WAIT / 60_000} minutes).`);
    await page.getByRole('button', { name: /^(Sign in without an organization name|Return to sign in)$/ }).click();
    await expect(signOut).toBeVisible({ timeout: SIGN_IN_WAIT });
    mkdirSync(dirname(STATE), { recursive: true, mode: 0o700 });
    await context.storageState({ path: STATE });
    chmodSync(STATE, 0o600);
  }
  return { context, page };
}

/** Confirms `name`, or the only organization when none is named, and
 * returns its ID. */
export async function chooseOrganization(page: Page, name: string | undefined): Promise<string> {
  const options = await organizationNames(page);
  const choice = name ?? (options.length === 1 ? options[0] : undefined);
  if (!choice || !options.includes(choice)) {
    throw new Error(`set LIVE_ORGANIZATION to one of: ${options.join(', ')}`);
  }
  await selectOrganization(page, choice);
  // Confirmed: the tab title names the organization.
  await expect(page).toHaveTitle(new RegExp(` · ${choice.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')} · `));
  return organizationId(page);
}
