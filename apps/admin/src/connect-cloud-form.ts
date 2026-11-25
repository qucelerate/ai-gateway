import type { ProviderFamily, ProviderSecretInput } from './api';

// What the Connect a provider flow asks for, checked before anything is
// sent, and the credential each choice stores: for a cloud, keyless, naming
// what the tenant created (a role, a directory, a service account); for
// Anthropic and OpenAI, an API key.

export type Cloud = 'aws' | 'azure' | 'google' | 'anthropic' | 'openai' | 'openai-compatible';

/** Choices that connect with an API key, which need no gateway cloud
 * identity and so are always offered. */
export type DirectAccount = Extract<Cloud, 'anthropic' | 'openai'>;

export const AWS_ROLE_NAME = 'QucelerateGateway';
export const GOOGLE_ACCOUNT_ID = 'qucelerate-gateway';

export function awsAccountId(value: string): string | null {
  const account = value.replace(/[\s-]/g, '');
  return /^\d{12}$/.test(account) ? account : null;
}

export function awsRegion(value: string): string | null {
  const region = value.trim();
  return /^[a-z]{2}(-[a-z]+)+-\d+$/.test(region) ? region : null;
}

export function googleProject(value: string): string | null {
  const project = value.trim();
  return /^[a-z][a-z0-9-]{4,28}[a-z0-9]$/.test(project) ? project : null;
}

export function uuid(value: string): string | null {
  const id = value.trim().toLowerCase();
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id) ? id : null;
}

/** `/subscriptions/<id>/resourceGroups/<group>/providers/Microsoft.CognitiveServices/accounts/<name>`: the account name. */
export function azureAccountName(resourceId: string): string | null {
  const match = /^\/subscriptions\/[0-9a-f-]{36}\/resourceGroups\/[\w.()-]+\/providers\/Microsoft\.CognitiveServices\/accounts\/([\w.-]+)$/i.exec(resourceId.trim());
  return match ? match[1] : null;
}

/** The endpoint inference calls for an Azure account, by family. */
export function azureEndpoint(accountName: string, family: ProviderFamily): string {
  return family === 'azure-ai-foundry'
    ? `https://${accountName.toLowerCase()}.services.ai.azure.com`
    : `https://${accountName.toLowerCase()}.openai.azure.com`;
}

/** Endpoints the gateway accepts for a tenant's resource of `family`, as
 * the server checks them: the family's own host or the Cognitive Services
 * one, in Azure, Azure Government or China. */
export function azureEndpointOk(value: string, family: ProviderFamily): boolean {
  const own = family === 'azure-ai-foundry' ? 'services\\.ai' : 'openai';
  // The resource name: 2 to 64 lowercase letters, digits and hyphens, not
  // starting with a hyphen.
  return new RegExp(`^https://[a-z0-9][a-z0-9-]{1,63}\\.(${own}|cognitiveservices)\\.azure\\.(com|us|cn)/?$`).test(value.trim());
}

/** `global`, `us-central1`, `eu`: 2 to 32 lowercase letters, digits and hyphens, starting with a letter. */
export function googleLocation(value: string): string | null {
  const location = value.trim();
  return /^[a-z][a-z0-9-]{1,31}$/.test(location) ? location : null;
}

/** A provider account name for the catalog: lowercase, from what it names. */
export function defaultAccountKey(cloud: Cloud, subject: string): string {
  const slug = subject.toLowerCase().replace(/[^a-z0-9]+/g, '-').replace(/^-|-$/g, '').slice(0, 40);
  const prefix = cloud === 'aws' ? 'bedrock' : cloud === 'azure' ? 'azure' : cloud === 'google' ? 'gemini' : cloud;
  return slug ? `${prefix}-${slug}` : prefix;
}

/** `key`, or the first of `key-2`, `key-3`, … no account in `taken` has. */
export function uniqueAccountKey(key: string, taken: ReadonlySet<string>): string {
  if (!taken.has(key)) return key;
  for (let n = 2; ; n += 1) {
    const candidate = `${key.slice(0, 127 - String(n).length)}-${n}`;
    if (!taken.has(candidate)) return candidate;
  }
}

/** What is wrong with an Anthropic or OpenAI API key as typed, if anything.
 * Only the shape is checked here; verification checks the key itself. */
export function apiKeyProblem(account: DirectAccount, value: string): string | null {
  const key = value.trim();
  if (!key) return 'Enter the API key.';
  if (/[\s\u0000-\u001f\u007f]/.test(key)) return 'Remove spaces and line breaks.';
  // Admin keys manage the organization and cannot call models.
  if (account === 'anthropic' && key.startsWith('sk-ant-admin')) return 'This is an Admin API key, which cannot call models. Create an API key under Console → API keys.';
  if (account === 'openai' && key.startsWith('sk-admin-')) return 'This is an Admin key, which cannot call models. Create a project API key under API keys.';
  return null;
}

export function apiKeyCredential(apiKey: string): ProviderSecretInput {
  return { api_key: apiKey.trim() };
}

export function accountKeyOk(value: string): boolean {
  return /^[a-z0-9][a-z0-9._-]{0,127}$/.test(value);
}

export function awsCredential(accountId: string, region: string): ProviderSecretInput {
  const partition = region.startsWith('us-gov-') ? 'aws-us-gov' : region.startsWith('cn-') ? 'aws-cn' : 'aws';
  return { role_arn: `arn:${partition}:iam::${accountId}:role/${AWS_ROLE_NAME}` };
}

/** With the resource's ID, discovery can list its deployments (the
 * instructions grant Reader on it). */
export function azureCredential(tenantId: string, endpoint: string, resourceId?: string): ProviderSecretInput {
  const credential: ProviderSecretInput = { tenant_id: tenantId, endpoint: endpoint.trim().replace(/\/$/, '') };
  if (resourceId && azureAccountName(resourceId)) credential.resource_id = resourceId.trim();
  return credential;
}

export function googleCredential(projectId: string, location: string): ProviderSecretInput {
  return {
    service_account: `${GOOGLE_ACCOUNT_ID}@${projectId}.iam.gserviceaccount.com`,
    project_id: projectId,
    location: location.trim() || 'global',
  };
}

/** An OpenAI-compatible endpoint's base URL, as the gateway stores it: http
 * or https, a host, no credentials, query or fragment, no trailing slash. */
export function openAiCompatibleEndpoint(value: string): string | null {
  let url: URL;
  try {
    url = new URL(value.trim());
  } catch {
    return null;
  }
  if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password || url.search || url.hash) return null;
  return url.href.replace(/\/+$/, '');
}

/** Whether the gateway can reach `endpoint` without the operator allowing
 * it: HTTPS to a name or public address. A host name that resolves to a
 * private address is only found out when the connection is checked. */
export function needsOperatorAllowance(endpoint: string, allowed: string[]): boolean {
  const url = new URL(endpoint);
  if (allowed.includes(url.origin)) return false;
  if (url.protocol !== 'https:') return true;
  const host = url.hostname.replace(/^\[|\]$/g, '');
  return host === 'localhost'
    || host.endsWith('.localhost')
    || host.endsWith('.internal')
    || host.endsWith('.local')
    || /^(10|127)\./.test(host)
    || /^192\.168\./.test(host)
    || /^172\.(1[6-9]|2\d|3[01])\./.test(host)
    || /^169\.254\./.test(host)
    || /^(::1|f[cd][0-9a-f]{2}:|fe80:)/i.test(host);
}

export function openAiCompatibleCredential(endpoint: string, apiKey: string): ProviderSecretInput {
  const credential: ProviderSecretInput = { endpoint };
  if (apiKey.trim()) credential.api_key = apiKey.trim();
  return credential;
}

/** The first label of the endpoint's host, for its default account name. */
export function endpointName(endpoint: string): string {
  return new URL(endpoint).hostname.split('.')[0] ?? '';
}

/** Where Microsoft's admin-consent return left the browser: `?connect=azure&organization=&tenant=` or `&error=`. */
export type ConsentReturn = { tenant: string | null; organization: string | null; error: string | null };

export function consentReturn(search: string): ConsentReturn | null {
  const params = new URLSearchParams(search);
  if (params.get('connect') !== 'azure') return null;
  return {
    tenant: uuid(params.get('tenant') ?? ''),
    organization: uuid(params.get('organization') ?? ''),
    error: params.get('error'),
  };
}
