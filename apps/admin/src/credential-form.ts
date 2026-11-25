import type { ProviderFamily, ProviderSecretInput } from './api';

// How each provider family can authenticate (ProviderSecretInput in
// openapi/admin-catalog-v1.json). Secret fields are never echoed back.
type Field = keyof ProviderSecretInput;
export type FieldSpec = {
  name: Field;
  label: string;
  secret?: boolean;
  optional?: boolean;
  hint?: string;
  pattern?: RegExp;
  example?: string;
  /** Normalize before checking and sending, as the server does. */
  lowercase?: boolean;
};
export type CredentialMode = {
  id: string;
  label: string;
  description: string;
  fields: FieldSpec[];
  /** Rules across fields, once each field is valid on its own. */
  check?: (secret: ProviderSecretInput) => Partial<Record<Field, string>>;
};

// `<name>@<project>.iam.gserviceaccount.com`, or a default service account
// (`<number>-compute@developer.gserviceaccount.com`, `<project>@appspot.gserviceaccount.com`).
const serviceAccountEmail = /^[a-z0-9-]{1,63}@(?:[a-z0-9-]{1,63}\.iam|developer|appspot)\.gserviceaccount\.com$/;
const uuid = /^[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12}$/;
const apiKey: CredentialMode = {
  id: 'api_key',
  label: 'API key',
  description: 'A key issued by the provider.',
  fields: [{ name: 'api_key', label: 'API key', secret: true }],
};
const awsRole: CredentialMode = {
  id: 'role',
  label: 'IAM role (recommended)',
  description: 'The gateway assumes a role in your AWS account for short-lived credentials. Its trust policy must allow the gateway with the external ID under Cloud trust.',
  fields: [{ name: 'role_arn', label: 'Role ARN', pattern: /^arn:aws(-cn|-us-gov)?:iam::[0-9]{12}:role\/.+$/, example: 'arn:aws:iam::123456789012:role/gateway-bedrock' }],
};
const awsKeys: CredentialMode = {
  id: 'keys',
  label: 'Access keys',
  description: 'Long-lived IAM user keys.',
  fields: [
    { name: 'access_key_id', label: 'Access key ID', secret: true },
    { name: 'secret_access_key', label: 'Secret access key', secret: true },
    { name: 'session_token', label: 'Session token', secret: true, optional: true },
  ],
};
const entra: CredentialMode = {
  id: 'entra',
  label: 'Microsoft Entra (recommended)',
  description: 'The gateway signs in to your directory with its own identity for short-lived tokens. It is used once the gateway operator binds your directory to this organization.',
  fields: [
    { name: 'tenant_id', label: 'Directory (tenant) ID', pattern: uuid },
    { name: 'client_id', label: 'Application (client) ID', optional: true, pattern: uuid, hint: 'An app in your directory whose federated credential trusts the gateway. Leave empty to use the gateway’s own app.' },
    { name: 'endpoint', label: 'Resource endpoint', optional: true, pattern: /^https:\/\/[^\s/]+\/?$/, example: 'https://my-resource.openai.azure.com' },
    { name: 'resource_id', label: 'Resource ID', optional: true, pattern: /^\/subscriptions\/[0-9a-f-]{36}\/resourceGroups\/[\w.()-]+\/providers\/Microsoft\.CognitiveServices\/accounts\/[\w.-]+$/i, example: '/subscriptions/…/resourceGroups/…/providers/Microsoft.CognitiveServices/accounts/my-resource', hint: 'Lets discovery list the resource’s deployments; needs the Reader role on it for the gateway.' },
  ],
};
const googleServiceAccount: CredentialMode = {
  id: 'service_account',
  label: 'Service account (recommended)',
  description: 'The gateway impersonates a service account in your Google Cloud project and calls Gemini on Agent Platform. It is used once the gateway operator binds the account to this organization.',
  fields: [
    { name: 'service_account', label: 'Service account email', pattern: serviceAccountEmail, lowercase: true, example: 'gateway@my-project.iam.gserviceaccount.com' },
    { name: 'project_id', label: 'Project ID', optional: true, pattern: /^[a-z][a-z0-9-]{4,28}[a-z0-9]$/, hint: 'Defaults to the service account’s project; required for a default (compute or App Engine) service account.' },
    { name: 'location', label: 'Location', optional: true, pattern: /^[a-z][a-z0-9-]{1,31}$/, hint: '`global` or a region such as us-central1.' },
  ],
  // A default service account's address names no project to bill.
  check: secret => (secret.service_account && !secret.service_account.endsWith('.iam.gserviceaccount.com') && !secret.project_id
    ? { project_id: 'Enter the project ID: a default service account’s address does not name its project.' }
    : {}),
};

export function credentialModes(family: ProviderFamily): CredentialMode[] {
  switch (family) {
    case 'bedrock': return [awsRole, awsKeys];
    case 'azure-openai':
    case 'azure-ai-foundry': return [entra, apiKey];
    case 'gemini': return [googleServiceAccount, apiKey];
    default: return [apiKey];
  }
}

/** The request body for a mode, or the errors that keep it from being sent. */
export function buildSecret(mode: CredentialMode, values: Partial<Record<Field, string>>): { secret: ProviderSecretInput } | { errors: Partial<Record<Field, string>> } {
  const secret: ProviderSecretInput = {};
  const errors: Partial<Record<Field, string>> = {};
  for (const field of mode.fields) {
    const trimmed = (values[field.name] ?? '').trim();
    const value = field.lowercase ? trimmed.toLowerCase() : trimmed;
    if (!value) {
      if (!field.optional) errors[field.name] = `${field.label} is required.`;
      continue;
    }
    if (field.secret && /[\s\u0000-\u001f\u007f]/.test(value)) errors[field.name] = 'Remove spaces and line breaks.';
    else if (field.pattern && !field.pattern.test(value)) errors[field.name] = field.example ? `Expected something like ${field.example}.` : `Check the ${field.label}.`;
    else secret[field.name] = value;
  }
  if (!Object.keys(errors).length && mode.check) Object.assign(errors, mode.check(secret));
  return Object.keys(errors).length ? { errors } : { secret };
}
