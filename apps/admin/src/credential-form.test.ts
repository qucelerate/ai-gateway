import { describe, expect, it } from 'vitest';
import { buildSecret, credentialModes } from './credential-form';

describe('credential form', () => {
  it('offers each family the ways it can authenticate, keyless first', () => {
    expect(credentialModes('openai').map(mode => mode.id)).toEqual(['api_key']);
    expect(credentialModes('bedrock').map(mode => mode.id)).toEqual(['role', 'keys']);
    expect(credentialModes('azure-ai-foundry').map(mode => mode.id)).toEqual(['entra', 'api_key']);
    expect(credentialModes('gemini').map(mode => mode.id)).toEqual(['service_account', 'api_key']);
  });

  it('sends only the filled fields of the chosen mode', () => {
    const [entra] = credentialModes('azure-openai');
    expect(buildSecret(entra, { tenant_id: ' 00000000-0000-4000-8000-000000000010 ', client_id: '', api_key: 'stray' }))
      .toEqual({ secret: { tenant_id: '00000000-0000-4000-8000-000000000010' } });
    const [, keys] = credentialModes('bedrock');
    expect(buildSecret(keys, { access_key_id: 'AKIA', secret_access_key: 's3cret' })).toEqual({ secret: { access_key_id: 'AKIA', secret_access_key: 's3cret' } });
  });

  it('accepts default Google service accounts, with the project they cannot name', () => {
    const [serviceAccount] = credentialModes('gemini');
    expect(buildSecret(serviceAccount, { service_account: 'gw@my-project.iam.gserviceaccount.com' })).toEqual({ secret: { service_account: 'gw@my-project.iam.gserviceaccount.com' } });
    for (const address of ['123456789-compute@developer.gserviceaccount.com', 'my-project@appspot.gserviceaccount.com']) {
      expect(buildSecret(serviceAccount, { service_account: address, project_id: 'my-project' })).toEqual({ secret: { service_account: address, project_id: 'my-project' } });
      expect(buildSecret(serviceAccount, { service_account: address })).toEqual({ errors: { project_id: 'Enter the project ID: a default service account’s address does not name its project.' } });
    }
    expect('errors' in buildSecret(serviceAccount, { service_account: 'gw@example.com' })).toBe(true);
    // Lowercased, as the server does before checking it.
    expect(buildSecret(serviceAccount, { service_account: 'Gemini@Tenant-Project.iam.gserviceaccount.com' })).toEqual({ secret: { service_account: 'gemini@tenant-project.iam.gserviceaccount.com' } });
  });

  it('explains what is missing or malformed', () => {
    const [role] = credentialModes('bedrock');
    expect(buildSecret(role, {})).toEqual({ errors: { role_arn: 'Role ARN is required.' } });
    expect('errors' in buildSecret(role, { role_arn: 'arn:aws:iam::1:role/x' })).toBe(true);
    const [apiKey] = credentialModes('openai');
    expect(buildSecret(apiKey, { api_key: 'sk-a b' })).toEqual({ errors: { api_key: 'Remove spaces and line breaks.' } });
  });
});
