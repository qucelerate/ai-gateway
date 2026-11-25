import { createHmac, randomBytes, randomUUID } from 'node:crypto';
import { readFileSync } from 'node:fs';
import type { BrowserContext } from '@playwright/test';
import pg from 'pg';

// A signed-in organization owner, written straight to the database: the
// suite runs without an OIDC provider (sign-in itself is covered by the
// Rust tests). A session is what the callback would have stored: digests,
// keyed with the stack's OIDC_SESSION_HMAC_SECRET, of a token and a browser
// binding that the browser then presents as cookies, with the CSRF cookie
// the UI echoes back in a header.

export type Owner = {
  organizationId: string;
  organizationName: string;
  slug: string;
  userId: string;
  cookies: { name: string; value: string; httpOnly: boolean }[];
};

function stackEnv(): Record<string, string> {
  const file = process.env.FUNCTIONAL_ENV_FILE;
  if (!file) throw new Error('FUNCTIONAL_ENV_FILE is not set; run the suite with tests/functional/run.sh');
  return Object.fromEntries(readFileSync(file, 'utf8').split('\n')
    .filter(line => line.includes('='))
    .map(line => [line.slice(0, line.indexOf('=')), line.slice(line.indexOf('=') + 1)]));
}

function opaqueToken(): string {
  return randomBytes(32).toString('base64url');
}

export async function seedOwner(name: string): Promise<Owner> {
  const env = stackEnv();
  const client = new pg.Client({
    host: '127.0.0.1',
    port: Number(process.env.POSTGRES_PORT ?? 15532),
    database: 'gateway',
    user: 'gateway_app',
    password: env.GATEWAY_APP_PASSWORD,
  });
  await client.connect();
  try {
    const organizationId = randomUUID();
    const userId = randomUUID();
    const slug = `${name}-${organizationId.slice(0, 8)}`;
    const organizationName = `Functional ${name}`;
    // run.sh writes the secret as base64url, which the gateway decodes.
    const key = Buffer.from(env.OIDC_SESSION_HMAC_SECRET, 'base64url');
    const digest = (value: string) => createHmac('sha256', key).update(value).digest();
    const token = opaqueToken();
    const browser = opaqueToken();
    await client.query('BEGIN');
    await client.query('INSERT INTO gateway.organization (id, slug, name) VALUES ($1, $2, $3)', [organizationId, slug, organizationName]);
    await client.query('INSERT INTO gateway.user_account (id, email, display_name) VALUES ($1, $2, $3)', [userId, `${slug}@example.test`, 'Functional Owner']);
    await client.query("INSERT INTO gateway.organization_membership (organization_id, user_id, role) VALUES ($1, $2, 'owner')", [organizationId, userId]);
    await client.query("INSERT INTO gateway.admin_role_binding (id, organization_id, user_id, role) VALUES ($1, $2, $3, 'organization_owner')", [randomUUID(), organizationId, userId]);
    await client.query(
      "INSERT INTO gateway.session (id, token_digest, user_id, browser_binding_digest, expires_at) VALUES ($1, $2, $3, $4, now() + interval '1 hour')",
      [randomUUID(), digest(token), userId, digest(browser)],
    );
    await client.query('COMMIT');
    return {
      organizationId,
      organizationName,
      slug,
      userId,
      cookies: [
        { name: '__Host-gateway_session', value: token, httpOnly: true },
        { name: '__Host-gateway_oidc_browser', value: browser, httpOnly: true },
        { name: '__Host-gateway_csrf', value: opaqueToken(), httpOnly: false },
      ],
    };
  } finally {
    await client.end();
  }
}

/** Gives `context` the owner's session. `__Host-` cookies are Secure, which
 * Chromium sends to http://localhost (a secure context) but sets only for an
 * https URL; the host is what matters. */
export async function signIn(context: BrowserContext, owner: Owner, baseURL: string): Promise<void> {
  const url = baseURL.replace(/^http:/, 'https:');
  await context.addCookies(owner.cookies.map(cookie => ({ ...cookie, url, secure: true, sameSite: 'Lax' as const })));
}
