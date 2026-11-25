import { describe, expect, it } from 'vitest';
import {
  apiKeyCredential,
  apiKeyProblem,
  defaultAccountKey as defaultKeyFor,
  uniqueAccountKey,
  googleLocation,
  accountKeyOk,
  awsAccountId,
  awsCredential,
  awsRegion,
  azureAccountName,
  azureCredential,
  azureEndpoint,
  azureEndpointOk,
  consentReturn,
  defaultAccountKey,
  googleCredential,
  googleProject,
} from './connect-cloud-form';

describe('connect a cloud form', () => {
  it('reads AWS accounts and regions', () => {
    expect(awsAccountId('1234-5678-9012')).toBe('123456789012');
    expect(awsAccountId('12345')).toBeNull();
    expect(awsRegion('us-east-2')).toBe('us-east-2');
    expect(awsRegion('us-gov-west-1')).toBe('us-gov-west-1');
    expect(awsRegion('US-EAST-1')).toBeNull();
    expect(awsCredential('123456789012', 'us-east-1')).toEqual({ role_arn: 'arn:aws:iam::123456789012:role/QucelerateGateway' });
    expect(awsCredential('123456789012', 'cn-north-1').role_arn).toMatch(/^arn:aws-cn:/);
  });

  it('derives the Azure account and its endpoint per family', () => {
    const resource = '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/Acme-OpenAI';
    expect(azureAccountName(resource)).toBe('Acme-OpenAI');
    expect(azureAccountName('/subscriptions/x/oops')).toBeNull();
    expect(azureEndpoint('Acme-OpenAI', 'azure-openai')).toBe('https://acme-openai.openai.azure.com');
    expect(azureEndpoint('acme', 'azure-ai-foundry')).toBe('https://acme.services.ai.azure.com');
    expect(azureEndpointOk('https://acme.cognitiveservices.azure.com/', 'azure-openai')).toBe(true);
    expect(azureEndpointOk('https://acme.openai.azure.us', 'azure-openai')).toBe(true);
    expect(azureEndpointOk('https://acme.services.ai.azure.com', 'azure-ai-foundry')).toBe(true);
    // Each family's own host only.
    expect(azureEndpointOk('https://acme.services.ai.azure.com', 'azure-openai')).toBe(false);
    expect(azureEndpointOk('https://acme.openai.azure.com', 'azure-ai-foundry')).toBe(false);
    expect(azureEndpointOk('https://evil.example.com', 'azure-openai')).toBe(false);
    expect(azureEndpointOk('http://acme.openai.azure.com', 'azure-openai')).toBe(false);
    expect(azureEndpointOk('https://acme.openai.azure.com.evil.example', 'azure-openai')).toBe(false);
    // The server's bounds on the resource name: 2 to 64 characters.
    expect(azureEndpointOk(`https://a.openai.azure.com`, 'azure-openai')).toBe(false);
    expect(azureEndpointOk(`https://ab.openai.azure.com`, 'azure-openai')).toBe(true);
    expect(azureEndpointOk(`https://${'a'.repeat(64)}.openai.azure.com`, 'azure-openai')).toBe(true);
    expect(azureEndpointOk(`https://${'a'.repeat(65)}.openai.azure.com`, 'azure-openai')).toBe(false);
    expect(azureEndpointOk('https://-acme.openai.azure.com', 'azure-openai')).toBe(false);
    expect(azureCredential('00000000-0000-4000-8000-000000000010', 'https://acme.openai.azure.com/')).toEqual({
      tenant_id: '00000000-0000-4000-8000-000000000010',
      endpoint: 'https://acme.openai.azure.com',
    });
    const resourceId = '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg-ai/providers/Microsoft.CognitiveServices/accounts/acme';
    expect(azureCredential('00000000-0000-4000-8000-000000000010', 'https://acme.openai.azure.com', ` ${resourceId} `)).toEqual({
      tenant_id: '00000000-0000-4000-8000-000000000010',
      endpoint: 'https://acme.openai.azure.com',
      resource_id: resourceId,
    });
    expect(azureCredential('00000000-0000-4000-8000-000000000010', 'https://acme.openai.azure.com', 'not a resource')).not.toHaveProperty('resource_id');
  });

  it('names the Google service account the instructions create', () => {
    expect(googleProject('acme-ai-123')).toBe('acme-ai-123');
    expect(googleProject('Acme')).toBeNull();
    expect(googleLocation('global')).toBe('global');
    expect(googleLocation('us-central1')).toBe('us-central1');
    expect(googleLocation('US Central')).toBeNull();
    expect(googleLocation('e')).toBeNull();
    expect(googleLocation(`e${'u'.repeat(31)}`)).toBe(`e${'u'.repeat(31)}`);
    expect(googleLocation(`e${'u'.repeat(32)}`)).toBeNull();
    expect(googleCredential('acme-ai-123', '')).toEqual({
      service_account: 'qucelerate-gateway@acme-ai-123.iam.gserviceaccount.com',
      project_id: 'acme-ai-123',
      location: 'global',
    });
  });

  it('suggests provider account names the catalog accepts', () => {
    expect(defaultAccountKey('aws', '123456789012')).toBe('bedrock-123456789012');
    expect(defaultAccountKey('azure', 'Acme-OpenAI')).toBe('azure-acme-openai');
    expect(defaultAccountKey('google', '')).toBe('gemini');
    expect(accountKeyOk('bedrock-123456789012')).toBe(true);
    expect(accountKeyOk('Bad Name')).toBe(false);
  });

  it('reads only well-formed consent returns', () => {
    expect(consentReturn('?connect=azure&organization=64bd1ec5-dbcc-4bbc-b0f4-9dcf3e1581f2&tenant=00000000-0000-4000-8000-0000000000AB')).toEqual({
      tenant: '00000000-0000-4000-8000-0000000000ab',
      organization: '64bd1ec5-dbcc-4bbc-b0f4-9dcf3e1581f2',
      error: null,
    });
    expect(consentReturn('?connect=azure&error=access_denied')?.error).toBe('access_denied');
    expect(consentReturn('?connect=azure&tenant=not-a-uuid')?.tenant).toBeNull();
    expect(consentReturn('?tab=models')).toBeNull();
  });
});

describe('OpenAI-compatible endpoints', () => {
  it('keeps a base URL the gateway stores, and nothing else', async () => {
    const { openAiCompatibleEndpoint } = await import('./connect-cloud-form');
    expect(openAiCompatibleEndpoint(' https://models.example.com/v1/ ')).toBe('https://models.example.com/v1');
    expect(openAiCompatibleEndpoint('http://qwen.models.svc.cluster.local:8080/v1')).toBe('http://qwen.models.svc.cluster.local:8080/v1');
    for (const invalid of ['models.example.com/v1', 'ftp://models.example.com', 'https://u:p@models.example.com/v1', 'https://models.example.com/v1?k=1', 'https://models.example.com/v1#x', '']) {
      expect(openAiCompatibleEndpoint(invalid), invalid).toBeNull();
    }
  });

  it('says when the operator has to allow an endpoint', async () => {
    const { needsOperatorAllowance } = await import('./connect-cloud-form');
    const allowed = ['http://qwen.models.svc.cluster.local:8080'];
    expect(needsOperatorAllowance('https://models.example.com/v1', allowed)).toBe(false);
    expect(needsOperatorAllowance('http://qwen.models.svc.cluster.local:8080/v1', allowed)).toBe(false);
    for (const endpoint of ['http://models.example.com/v1', 'https://10.1.2.3/v1', 'https://192.168.1.4/v1', 'https://172.20.0.1/v1', 'https://localhost:8443/v1', 'https://[::1]/v1', 'https://gpu.internal/v1', 'https://qwen.svc.cluster.local/v1']) {
      expect(needsOperatorAllowance(endpoint, allowed), endpoint).toBe(true);
    }
  });

  it('sends a key only when one is entered', async () => {
    const { openAiCompatibleCredential, defaultAccountKey, endpointName } = await import('./connect-cloud-form');
    expect(openAiCompatibleCredential('https://m.example.com/v1', ' ')).toEqual({ endpoint: 'https://m.example.com/v1' });
    expect(openAiCompatibleCredential('https://m.example.com/v1', ' sk ')).toEqual({ endpoint: 'https://m.example.com/v1', api_key: 'sk' });
    expect(defaultAccountKey('openai-compatible', endpointName('http://qwen3-coder.qwen3-coder.svc:8080/v1'))).toBe('openai-compatible-qwen3-coder');
  });

  it('checks Anthropic and OpenAI API keys by shape only', () => {
    expect(apiKeyProblem('anthropic', '')).toBe('Enter the API key.');
    expect(apiKeyProblem('openai', '   ')).toBe('Enter the API key.');
    expect(apiKeyProblem('anthropic', 'sk-ant-api03 x')).toBe('Remove spaces and line breaks.');
    expect(apiKeyProblem('openai', 'sk-proj-a\nb')).toBe('Remove spaces and line breaks.');
    expect(apiKeyProblem('anthropic', 'sk-ant-admin01-x')).toMatch(/Admin API key/);
    expect(apiKeyProblem('openai', 'sk-admin-x')).toMatch(/Admin key/);
    expect(apiKeyProblem('anthropic', ' sk-ant-api03-x ')).toBeNull();
    expect(apiKeyProblem('openai', 'sk-proj-x')).toBeNull();
    // Another provider's key is the provider's to refuse.
    expect(apiKeyProblem('openai', 'sk-ant-api03-x')).toBeNull();
    expect(apiKeyCredential(' sk-proj-x ')).toEqual({ api_key: 'sk-proj-x' });
  });

  it('names API-key accounts after the provider, never one already taken', () => {
    expect(defaultKeyFor('anthropic', 'main')).toBe('anthropic-main');
    expect(defaultKeyFor('openai', 'main')).toBe('openai-main');
    expect(uniqueAccountKey('openai-main', new Set())).toBe('openai-main');
    expect(uniqueAccountKey('openai-main', new Set(['openai-main', 'openai-main-2']))).toBe('openai-main-3');
    expect(uniqueAccountKey('a'.repeat(128), new Set(['a'.repeat(128)]))).toBe(`${'a'.repeat(126)}-2`);
  });
});
