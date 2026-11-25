import { describe, expect, it } from 'vitest';
import type { ModelEvidence } from './api';
import { accountKind, accountRegion, actionFor, bedrockCatalogLink, blockedQuery, callsText, deployedQuery, endpointText, retirements, servesText, skuText, undeployedQuery } from './availability-model';

const evidence = (state: ModelEvidence['state'], reason: ModelEvidence['reason'] = null): ModelEvidence => ({
  state, reason, source: 'probe', deployment: null, provider_status: null, provider_code: null, observed_at: '2026-11-05T00:00:00Z', verified_at: null,
});

describe('availability queries', () => {
  it('lists every Azure deployment as deployed, whatever its evidence', () => {
    expect(accountKind('azure-openai')).toBe('deployments');
    expect(accountKind('azure-ai-foundry')).toBe('deployments');
    expect(deployedQuery('foundry', 'deployments')).toEqual({ adapter_instance_key: 'foundry', state: 'listed' });
    expect(deployedQuery('foundry', 'deployments', ['requires_action'])).toEqual({ adapter_instance_key: 'foundry', state: 'listed', evidence: ['requires_action'] });
  });

  it('keeps blocked serverless models out of Deployed and in Deployable', () => {
    expect(accountKind('bedrock')).toBe('serverless');
    expect(accountKind('gemini')).toBe('serverless');
    expect(deployedQuery('bedrock-1', 'serverless').evidence).toEqual(['verified', 'entitled', 'none']);
    // A state Deployed does not allow falls back to all it does allow.
    expect(deployedQuery('bedrock-1', 'serverless', ['refused']).evidence).toEqual(['verified', 'entitled', 'none']);
    expect(deployedQuery('bedrock-1', 'serverless', ['none']).evidence).toEqual(['none']);
    expect(blockedQuery('bedrock-1')).toEqual({ adapter_instance_key: 'bedrock-1', state: 'listed', evidence: ['requires_action', 'refused'] });
  });

  it('asks Azure for catalog models no deployment serves', () => {
    expect(undeployedQuery('foundry', { operation: 'chat' })).toEqual({ adapter_instance_key: 'foundry', deployed: false, operation: 'chat' });
  });
});

describe('what to do about a blocked model', () => {
  it('sends a Bedrock agreement to the model catalog in the account’s region', () => {
    const action = actionFor('bedrock', evidence('requires_action', 'agreement_required'), { region: 'us-east-2' })!;
    expect(action.action).toContain('Request access');
    expect(action.link?.href).toBe('https://us-east-2.console.aws.amazon.com/bedrock/home?region=us-east-2#/model-catalog');
  });

  it('names the Azure roles for a missing permission', () => {
    const action = actionFor('azure-ai-foundry', evidence('requires_action', 'permission_missing'), { region: null })!;
    expect(action.action).toContain('Cognitive Services OpenAI User');
    expect(action.link?.href).toBe('https://portal.azure.com/');
  });

  it('names the IAM actions on AWS, and the quota on Google', () => {
    expect(actionFor('bedrock', evidence('requires_action', 'permission_missing'), { region: null })!.action).toContain('bedrock:InvokeModel');
    const quota = actionFor('gemini', evidence('requires_action', 'quota_zero'), { region: null })!;
    expect(quota.action).toContain('online_prediction_requests_per_base_model');
    expect(quota.link?.href).toContain('console.cloud.google.com/iam-admin/quotas');
    const policy = actionFor('gemini', evidence('requires_action', 'policy_denied'), { region: null })!;
    expect(policy.action).toContain('vertexai.allowedModels');
    expect(policy.link?.href).toContain('orgpolicies/vertexai-allowedModels');
  });

  it('sends a partner model the project has not enabled to Model Garden', () => {
    const action = actionFor('gemini', evidence('requires_action', 'enable_in_model_garden'), { region: null })!;
    expect(action.action).toContain('enable it');
    expect(action.link?.href).toBe('https://console.cloud.google.com/vertex-ai/model-garden');
  });

  it('says how the gateway calls partner models and endpoints', () => {
    const deployment = (format: string | null) => ({ format, model: null, version: null, sku: null, capacity: null });
    expect(callsText({ model: 'anthropic/claude-haiku-4-5', availability: 'unknown', deployment: deployment('anthropic_messages') }))
      .toBe('Through Anthropic’s Messages API (rawPredict); location not confirmed');
    expect(callsText({ model: 'endpoints/12', availability: 'listed', deployment: deployment('openai_chat') }))
      .toBe('Through OpenAI chat completions on the endpoint');
    expect(callsText({ model: 'endpoints/13', availability: 'endpoint_only', deployment: deployment(null) }))
      .toContain('Not yet');
    expect(callsText({ model: 'gemini-2.5-flash', availability: 'unknown', deployment: null })).toBe('Location not confirmed');
    expect(callsText({ model: 'amazon.nova-micro-v1:0', availability: 'listed', deployment: null })).toBe('Directly');
  });

  it('tells a 404 from a 403', () => {
    expect(actionFor('gemini', evidence('refused', 'not_found'), { region: null })!.action).toContain('global');
    expect(actionFor('bedrock', evidence('refused', 'access_denied'), { region: null })!.action).toContain('AWS account team');
  });

  it('has nothing to do for a model a call or check allows', () => {
    expect(actionFor('bedrock', evidence('verified'), { region: null })).toBeNull();
    expect(actionFor('bedrock', evidence('entitled'), { region: null })).toBeNull();
    expect(actionFor('bedrock', null, { region: null })).toBeNull();
  });
});

describe('console links and Azure details', () => {
  it('takes a region only when the account’s deployments agree on one', () => {
    const deployments = [
      { adapter_instance_key: 'bedrock-1', location: 'us-east-2' },
      { adapter_instance_key: 'bedrock-1', location: 'us-east-2' },
      { adapter_instance_key: 'bedrock-2', location: 'eu-west-1' },
    ];
    expect(accountRegion('bedrock-1', deployments)).toBe('us-east-2');
    expect(accountRegion('bedrock-1', [...deployments, { adapter_instance_key: 'bedrock-1', location: 'us-west-2' }])).toBeNull();
    expect(accountRegion('bedrock-3', deployments)).toBeNull();
  });

  it('never puts an unexpected region into a link', () => {
    expect(bedrockCatalogLink('evil.example/x').href).toBe('https://console.aws.amazon.com/bedrock/home#/model-catalog');
    expect(bedrockCatalogLink(null).href).toBe('https://console.aws.amazon.com/bedrock/home#/model-catalog');
  });

  it('finds the earliest retirement of each deployment', () => {
    const byName = retirements([
      { deployments: ['gpt-a'], retires_at: '2027-03-01T00:00:00Z' },
      { deployments: ['gpt-a', 'gpt-b'], retires_at: '2027-01-01T00:00:00Z' },
      { deployments: ['gpt-c'], retires_at: null },
    ]);
    expect(byName.get('gpt-a')).toBe('2027-01-01T00:00:00Z');
    expect(byName.get('gpt-b')).toBe('2027-01-01T00:00:00Z');
    expect(byName.has('gpt-c')).toBe(false);
  });

  it('describes what a deployment serves and its SKU', () => {
    const model = { deployment: { format: 'OpenAI', model: 'gpt-5.4', version: '2026-03-01', sku: 'GlobalStandard', capacity: 50 } };
    expect(servesText(model)).toBe('OpenAI · gpt-5.4 2026-03-01');
    expect(skuText(model)).toBe('GlobalStandard, capacity 50');
    expect(servesText({ deployment: null })).toBeNull();
    expect(skuText({ deployment: { format: null, model: null, version: null, sku: null, capacity: null } })).toBeNull();
  });

  it('describes what a Google endpoint serves, on what machines and replicas', () => {
    const endpoint = { deployment: { format: null, model: 'gemma-3-1b-it', version: '1', sku: 'g2-standard-12 + 1 NVIDIA_L4', capacity: 1 } };
    expect(endpointText(endpoint)).toBe('gemma-3-1b-it 1 on g2-standard-12 + 1 NVIDIA_L4, 1 replica');
    expect(endpointText({ deployment: { format: null, model: 'tuned-9', version: null, sku: null, capacity: 3 } })).toBe('tuned-9, 3 replicas');
    expect(endpointText({ deployment: null })).toBeNull();
  });
});
