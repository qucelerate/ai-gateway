import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ConnectCloud } from './connect-cloud';

const ORG = '64bd1ec5-dbcc-4bbc-b0f4-9dcf3e1581f2';
const TENANT = '00000000-0000-4000-8000-000000000010';
const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const summary = {
  aws: { principal_arn: 'arn:aws:iam::123456789012:user/example-gateway', external_id: `qucelerate-${ORG}`, role_name: 'QucelerateGateway' },
  azure: { client_id: '00000000-0000-4000-8000-000000000020' },
  google: { service_account: 'gateway-dev@gw.iam.gserviceaccount.com' },
};
const connected = { status: 'connected', message: 'The gateway signed in and can see 2 models.', model_count: 2, models: [{ model: 'anthropic.claude', display_name: null }, { model: 'us.amazon.nova-micro-v1:0', display_name: 'Nova Micro' }], region: 'us-east-2' };

const baseManifest = {
  release: { version: 'v1' },
  providers: [{ adapter_instance_key: 'openai-main', family: 'openai' }],
  deployments: [{ name: 'gpt', model: 'gpt-4o', location: null, adapter_instance_key: 'openai-main', capabilities: [{ operation: 'chat', max_input_bytes: 4096, max_output_tokens: 32, max_tokens_per_byte: 1, tokenizer: 'configured', reservation_ttl_seconds: 17, chat_input_count_mode: 'unsupported' }] }],
  prices: [{ deployment: 'gpt', operation: 'chat', input_minor_per_token: 250, output_minor_per_token: 1000, cached_input_minor_per_token: 125, reasoning_minor_per_token: 0, other_minor_per_token: 0, currency: 'USD', source: 'test', source_version: 'v1', confidence: 'known' }],
  routes: [{ alias: 'smart', operation: 'chat', deployment: 'gpt' }],
};

describe('connect a cloud', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let accounts: unknown[];
  let activation: () => Response | Promise<Response>;
  let accountsFail: boolean;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    accounts = [];
    activation = () => json({ release: { id: 'rel-2' }, activation_version: 4 });
    accountsFail = false;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/onboarding')) return json(summary);
      if (url.includes('/onboarding/aws?')) {
        const accept = url.includes('accept_agreements=true');
        return json({ role_arn: 'arn:aws:iam::123456789012:role/QucelerateGateway', external_id: `qucelerate-${ORG}`, launch_url: 'https://us-east-2.console.aws.amazon.com/cloudformation/home?region=us-east-2#/stacks/quickcreate?templateURL=x', template_url: 'x', accept_model_agreements: accept, actions: accept ? ['bedrock:InvokeModel', 'aws-marketplace:Subscribe'] : ['bedrock:InvokeModel'], cli: accept ? 'aws iam create-role ... aws-marketplace:Subscribe' : 'aws iam create-role ...', terraform: 'resource "aws_iam_role" "x" {}' });
      }
      if (url.includes('/onboarding/azure')) {
        const role = url.includes('family=azure-ai-foundry') ? 'Cognitive Services User' : 'Cognitive Services OpenAI User';
        return json({ consent_url: `https://login.microsoftonline.com/organizations/v2.0/adminconsent?state=${ORG}`, client_id: summary.azure.client_id, role, cli: `az role assignment create --role "${role}"`, terraform: 'resource "azurerm_role_assignment" "x" {}' });
      }
      if (url.includes('/onboarding/google?')) {
        return json({ service_account: 'qucelerate-gateway@acme-ai-123.iam.gserviceaccount.com', project_id: 'acme-ai-123', gcloud: 'gcloud iam service-accounts create ...', cloud_shell_url: 'https://shell.cloud.google.com/?cloudshell_project=acme-ai-123', terraform: 'resource "google_service_account" "x" {}' });
      }
      if (url.includes('/onboarding/verify')) return json(connected);
      if (url.endsWith('/catalog/releases/rel-1/manifest')) return json(baseManifest);
      if (url.endsWith('/activate')) return activation();
      if (url.endsWith('/catalog/releases') && init?.method === 'POST') {
        const manifest = JSON.parse(String(init.body));
        return json({ id: 'rel-2', version: manifest.release.version, digest: 'd', created_at: '', sealed_at: '', active: false, activated_at: null }, 201);
      }
      if (url.includes('/catalog/releases?')) return json({ items: [{ id: 'rel-1', version: 'v1', digest: 'd', created_at: '', sealed_at: '', active: true, activated_at: '' }], next_page_token: null, activation_version: 3 });
      if (url.includes('/price-observations?')) return json({ items: [{ source: 'aws-price-list:bedrock', snapshot_id: 's', item: 'Claude 3 Haiku', model: null, region: 'us-east-2', currency: 'USD', per_tokens: 1000, input: '0.00025', output: '0.00125', cached_input: null, reasoning: null, other: null }], next_page_token: null });
      if (url.includes('/provider-accounts') && accountsFail) return json({}, 500);
      if (url.includes('/provider-accounts')) return json({ release_id: 'r', items: accounts, next_page_token: null });
      if (url.includes('/catalog/credentials') && init?.method === 'PUT') return json({ adapter_instance_key: 'bedrock-main', family: 'bedrock', key_id: '0123456789abcdef', version: 1, created_at: '', rotated_at: '' }, 201);
      if (url.includes('/catalog/credentials')) return json({ items: [], next_page_token: null });
      return json({}, 404);
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
    window.history.replaceState(null, '', '/');
  });

  const settle = async () => {
    for (let i = 0; i < 8; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const render = async () => {
    act(() => root.render(<ConnectCloud organizationId={ORG} />));
    await settle();
  };
  const button = (text: string) => Array.from(container.querySelectorAll('button')).find(node => node.textContent?.startsWith(text)) as HTMLButtonElement;
  const setInput = (id: string, value: string) => {
    const input = document.getElementById(id) as HTMLInputElement;
    act(() => {
      Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value')!.set!.call(input, value);
      input.dispatchEvent(new Event('input', { bubbles: true }));
    });
    return input;
  };
  const submit = async (input: HTMLElement) => {
    act(() => ((input as HTMLInputElement).form as HTMLFormElement).requestSubmit());
    await settle();
  };
  const verifyBodies = () => fetchMock.mock.calls
    .filter(([url]) => String(url).includes('/onboarding/verify'))
    .map(([, init]) => JSON.parse(String((init as RequestInit).body)));

  it('offers only the clouds the gateway can be trusted in', async () => {
    fetchMock.mockImplementation(async () => json({ aws: null, azure: null, google: summary.google }));
    await render();
    expect(Array.from(container.querySelectorAll('.cloud-choice')).map(node => node.querySelector('strong')?.textContent)).toEqual(['Anthropic', 'OpenAI', 'Google Cloud']);
  });

  it('always offers Anthropic and OpenAI, which need no cloud identity', async () => {
    fetchMock.mockImplementation(async () => json({ aws: null, azure: null, google: null }));
    await render();
    expect(Array.from(container.querySelectorAll('.cloud-choice')).map(node => node.querySelector('strong')?.textContent)).toEqual(['Anthropic', 'OpenAI']);
    expect(container.querySelector('h2')?.textContent).toBe('Connect a provider');
    expect(container.textContent).not.toContain('No cloud onboarding');
    expect(container.textContent).toContain('Cloud accounts (AWS, Azure, Google Cloud) are offered once this gateway’s operator configures');
  });

  it('connects an OpenAI-compatible endpoint, with the operator\'s private origins offered', async () => {
    const origin = 'http://qwen38-27b-lb.qwen38-27b.svc.cluster.local:8080';
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (url.endsWith('/onboarding')) return json({ aws: null, azure: null, google: null, openai_compatible: { private_endpoints: [origin] } });
      if (url.includes('/onboarding/verify')) return json({ status: 'connected', message: 'The gateway signed in and can see 1 models.', model_count: 1, models: [{ model: 'qwen3.8-27b', display_name: null }], region: null });
      if (url.includes('/provider-accounts')) return json({ release_id: 'r', items: [], next_page_token: null });
      return json({}, init?.method ? 500 : 404);
    });
    await render();
    expect(Array.from(container.querySelectorAll('.cloud-choice')).map(node => node.querySelector('strong')?.textContent)).toEqual(['Anthropic', 'OpenAI', 'OpenAI-compatible endpoint']);
    act(() => button('OpenAI-compatible endpoint').click());
    expect(Array.from(container.querySelectorAll('#compatible-allowed option')).map(node => node.getAttribute('value'))).toEqual([`${origin}/v1`]);
    // Not a URL: no check is offered.
    setInput('compatible-endpoint', 'models/v1');
    expect(container.textContent).toContain('Enter an http or https URL');
    expect(button('Verify connection')).toBeUndefined();
    // Private and not allowed: the check is offered, with the operator's part explained.
    setInput('compatible-endpoint', 'http://10.0.0.5:8080/v1');
    expect(container.textContent).toContain('only if this gateway’s operator allows its origin');
    // An allowed origin needs no explanation.
    setInput('compatible-endpoint', `${origin}/v1/`);
    expect(container.textContent).not.toContain('only if this gateway’s operator allows its origin');
    setInput('compatible-key', 'sk-local');
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0]).toEqual({
      adapter_instance_key: 'openai-compatible-qwen38-27b-lb',
      family: 'openai-compatible',
      credential: { endpoint: `${origin}/v1`, api_key: 'sk-local' },
    });
    expect(container.textContent).toContain('Connected.');
    expect(container.textContent).toContain('Add it as a provider');
    // Without a key, none is sent.
    setInput('compatible-key', '');
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[1].credential).toEqual({ endpoint: `${origin}/v1` });
  });

  it('connects an AWS account: instructions, verification, then the catalog', async () => {
    await render();
    act(() => button('Amazon Web Services').click());
    setInput('aws-account', '1234-5678-9012');
    await submit(setInput('aws-region', 'us-east-2'));
    expect(fetchMock.mock.calls.map(([url]) => String(url))).toContain(`/api/admin/v1/organizations/${ORG}/onboarding/aws?account_id=123456789012&region=us-east-2`);
    const launch = Array.from(container.querySelectorAll('a')).find(node => node.textContent === 'Launch in AWS CloudFormation');
    expect(launch?.getAttribute('href')).toContain('quickcreate');
    expect(container.querySelector('pre.script')?.textContent).toContain('aws iam create-role');
    act(() => button('Terraform').click());
    expect(container.querySelector('pre.script')?.textContent).toContain('aws_iam_role');

    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0]).toEqual({
      adapter_instance_key: 'bedrock-123456789012',
      family: 'bedrock',
      credential: { role_arn: 'arn:aws:iam::123456789012:role/QucelerateGateway' },
    });
    expect(container.textContent).toContain('Connected.');
    expect(container.textContent).toContain('Region: us-east-2');
    // Not declared by the catalog yet: it is offered to add, not saved.
    expect(container.textContent).toContain('Add it as a provider');
    expect(button('Save for')).toBeUndefined();
  });

  it('lets the role accept model agreements only when asked', async () => {
    await render();
    act(() => button('Amazon Web Services').click());
    setInput('aws-account', '123456789012');
    const box = document.getElementById('aws-accept-agreements') as HTMLInputElement;
    expect(box.checked).toBe(false);
    await submit(setInput('aws-region', 'us-east-2'));
    expect(container.textContent).toContain('It can invoke and list models, nothing else.');
    expect(container.textContent).toContain('need access AWS grants on request');
    act(() => box.click());
    await submit(box);
    const urls = fetchMock.mock.calls.map(([url]) => String(url)).filter(url => url.includes('/onboarding/aws?'));
    expect(urls[0]).not.toContain('accept_agreements');
    expect(urls[1]).toContain('accept_agreements=true');
    expect(container.textContent).toContain('accept a model’s AWS Marketplace agreement when an administrator asks');
    expect(container.textContent).toContain('first ordinary call can then also create its subscription');
    expect(container.querySelector('pre.script')?.textContent).toContain('aws-marketplace:Subscribe');
  });

  it('adds the optional batch sections only when they are filled in', async () => {
    await render();
    act(() => button('Amazon Web Services').click());
    setInput('aws-account', '123456789012');
    setInput('aws-region', 'us-east-2');
    // A bucket without a role is asked to be completed, not sent.
    await submit(setInput('aws-batch-bucket', 'acme-batches'));
    expect(container.textContent).toContain('Enter the batch service role’s ARN, or clear the bucket.');
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/onboarding/aws?'))).toBe(false);
    await submit(setInput('aws-batch-role', 'arn:aws:iam::123456789012:role/BedrockBatch'));
    const aws = fetchMock.mock.calls.map(([url]) => String(url)).filter(url => url.includes('/onboarding/aws?'));
    expect(aws).toEqual([`/api/admin/v1/organizations/${ORG}/onboarding/aws?account_id=123456789012&region=us-east-2&batch_bucket=acme-batches&batch_role_arn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2FBedrockBatch`]);

    act(() => button('Microsoft Azure').click());
    await settle();
    const resource = '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme';
    setInput('azure-resource', resource);
    const box = document.getElementById('azure-batch') as HTMLInputElement;
    expect(box.checked).toBe(false);
    act(() => box.click());
    await submit(box);
    expect(fetchMock.mock.calls.map(([url]) => String(url)).filter(url => url.includes('/onboarding/azure?')).pop()).toContain('batch=true');
    // Foundry runs no batches, so it is not offered.
    const service = document.getElementById('azure-family') as HTMLSelectElement;
    act(() => {
      service.value = 'azure-ai-foundry';
      service.dispatchEvent(new Event('change', { bubbles: true }));
    });
    expect(document.getElementById('azure-batch')).toBeNull();

    act(() => button('Google Cloud').click());
    setInput('google-project', 'acme-ai-123');
    await submit(setInput('google-batch-bucket', 'acme-batches'));
    expect(fetchMock.mock.calls.map(([url]) => String(url)).filter(url => url.includes('/onboarding/google?')).pop()).toBe(`/api/admin/v1/organizations/${ORG}/onboarding/google?project_id=acme-ai-123&batch_bucket=acme-batches`);
  });

  it('saves a verified connection for a provider account the catalog declares', async () => {
    accounts = [{ id: 'a', release_id: 'r', adapter_instance_key: 'bedrock-main', family: 'bedrock', deployment_count: 2 }];
    await render();
    act(() => button('Amazon Web Services').click());
    await submit(setInput('aws-account', '123456789012'));
    const select = document.getElementById('connect-key') as HTMLSelectElement;
    act(() => {
      select.value = 'bedrock-main';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0].adapter_instance_key).toBe('bedrock-main');
    act(() => button('Save for bedrock-main').click());
    await settle();
    const [[url, init]] = fetchMock.mock.calls.filter(([, request]) => (request as RequestInit | undefined)?.method === 'PUT');
    expect(url).toBe(`/api/admin/v1/organizations/${ORG}/catalog/credentials/bedrock-main`);
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ role_arn: 'arn:aws:iam::123456789012:role/QucelerateGateway' });
    expect(new Headers((init as RequestInit).headers).get('If-Match')).toBe('*');
    expect(container.textContent).toContain('Saved for bedrock-main');
    // It checks what the account offers, and says what is left to do.
    const started = fetchMock.mock.calls.find(([address, request]) => String(address).endsWith('/catalog/sync-jobs') && (request as RequestInit | undefined)?.method === 'POST');
    expect(JSON.parse(String((started![1] as RequestInit).body))).toEqual({ adapter_instance_key: 'bedrock-main' });
    const links = Array.from(container.querySelectorAll('.next-steps a')).map(node => node.getAttribute('href'));
    expect(links).toEqual(['/admin/policies/budgets', '/admin/identities', '/admin/models/availability']);
  });

  it('continues Azure after consent with the directory filled in, then verifies its resource', async () => {
    window.history.replaceState(null, '', `/admin/providers?connect=azure&organization=${ORG}&tenant=${TENANT}`);
    await render();
    // The consent return is consumed and the address cleaned.
    expect(window.location.search).toBe('');
    expect(container.textContent).toContain(`Consent granted for directory ${TENANT}`);
    await submit(setInput('azure-resource', '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme'));
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/onboarding/azure?resource_id=%2Fsubscriptions'))).toBe(true);
    expect((document.getElementById('azure-tenant') as HTMLInputElement).value).toBe(TENANT);
    const consent = Array.from(container.querySelectorAll('a')).find(node => node.textContent === 'Grant consent in Microsoft Entra');
    expect(consent?.getAttribute('href')).toContain('adminconsent');
    expect((document.getElementById('azure-endpoint') as HTMLInputElement).value).toBe('https://acme.openai.azure.com');
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0]).toEqual({
      adapter_instance_key: 'azure-acme',
      family: 'azure-openai',
      // The resource ID too, so discovery can list its deployments.
      credential: { tenant_id: TENANT, endpoint: 'https://acme.openai.azure.com', resource_id: '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme' },
    });
  });

  it('shows Azure one step at a time, as AWS and Google', async () => {
    await render();
    act(() => button('Microsoft Azure').click());
    await settle();
    // Only the resource, until its instructions are asked for.
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/onboarding/azure'))).toBe(false);
    expect(document.getElementById('azure-tenant')).toBeNull();
    expect(container.textContent).not.toContain('Grant consent in Microsoft Entra');
    // The endpoint follows the resource, behind a disclosure.
    const resource = '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme';
    setInput('azure-resource', resource);
    const more = container.querySelector('details.connect-more') as HTMLDetailsElement;
    expect(more.open).toBe(false);
    expect(more.querySelector('summary')?.textContent).toBe('Endpoint: https://acme.openai.azure.com');
    await submit(document.getElementById('azure-resource') as HTMLInputElement);
    expect(container.textContent).toContain('Approve the gateway and give it access');
    expect(container.querySelector('pre.script')?.textContent).toContain('az role assignment create');
    // No check until the directory is known.
    expect(container.textContent).not.toContain('Check the connection');
    setInput('azure-tenant', TENANT);
    expect(container.textContent).toContain('Check the connection');
    // A different resource hides what was shown for the last one.
    setInput('azure-resource', resource.replace('Acme', 'Other'));
    expect(container.textContent).not.toContain('Approve the gateway and give it access');
  });

  it('resumes Azure where it was left when Microsoft sends the person back', async () => {
    const resource = '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme';
    await render();
    act(() => button('Microsoft Azure').click());
    await settle();
    const select = document.getElementById('azure-family') as HTMLSelectElement;
    act(() => {
      select.value = 'azure-ai-foundry';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await submit(setInput('azure-resource', resource));
    const consent = Array.from(container.querySelectorAll('a')).find(node => node.textContent === 'Grant consent in Microsoft Entra')!;
    // Leaving for Microsoft (the navigation itself is not followed here).
    consent.addEventListener('click', event => event.preventDefault());
    act(() => consent.click());
    act(() => root.unmount());
    root = createRoot(container);
    window.history.replaceState(null, '', `/admin/providers?connect=azure&organization=${ORG}&tenant=${TENANT}`);
    await render();
    expect((document.getElementById('azure-family') as HTMLSelectElement).value).toBe('azure-ai-foundry');
    expect((document.getElementById('azure-resource') as HTMLInputElement).value).toBe(resource);
    expect((document.getElementById('azure-tenant') as HTMLInputElement).value).toBe(TENANT);
    expect(container.textContent).toContain('Check the connection');
    // Taken once: a later visit starts empty.
    expect(sessionStorage.getItem('gateway-connect-azure')).toBeNull();
  });

  it('reports refused consent', async () => {
    window.history.replaceState(null, '', '/admin/providers?connect=azure&error=access_denied');
    await render();
    expect(container.textContent).toContain('Consent was not granted (access_denied)');
  });

  it('connects a Google project and explains an awaiting approval', async () => {
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) =>
      String(url).includes('/onboarding/verify')
        ? json({ status: 'awaiting_approval', message: 'The gateway’s operator must approve this Google service account for your organization before the gateway signs in to it. Nothing was contacted.' })
        : listing(url, init));
    await render();
    act(() => button('Google Cloud').click());
    await submit(setInput('google-project', 'acme-ai-123'));
    expect(container.textContent).toContain('lets the gateway (gateway-dev@gw.iam.gserviceaccount.com) act as it');
    expect(Array.from(container.querySelectorAll('a')).find(node => node.textContent === 'Open Cloud Shell')?.getAttribute('href')).toContain('cloudshell_project=acme-ai-123');
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0]).toEqual({
      adapter_instance_key: 'gemini-acme-ai-123',
      family: 'gemini',
      credential: { service_account: 'qucelerate-gateway@acme-ai-123.iam.gserviceaccount.com', project_id: 'acme-ai-123', location: 'global' },
    });
    expect(container.textContent).toContain('Awaiting the operator’s approval.');
  });

  it('renders the Foundry role when Foundry is chosen', async () => {
    await render();
    act(() => button('Microsoft Azure').click());
    await settle();
    const select = document.getElementById('azure-family') as HTMLSelectElement;
    act(() => {
      select.value = 'azure-ai-foundry';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await submit(setInput('azure-resource', '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme'));
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('family=azure-ai-foundry'))).toBe(true);
    expect(container.querySelector('pre.script')?.textContent).toContain('"Cognitive Services User"');
    expect(container.textContent).not.toContain('OpenAI User');
    expect((document.getElementById('azure-endpoint') as HTMLInputElement).value).toBe('https://acme.services.ai.azure.com');
  });

  it('keeps consent for another organization until it is selected', async () => {
    const other = '11111111-1111-1111-1111-111111111111';
    window.history.replaceState(null, '', `/admin/providers?connect=azure&organization=${other}&tenant=${TENANT}`);
    await render();
    expect(container.textContent).toContain('Microsoft returned consent for another of your organizations');
    // Not consumed: still in the address, and not applied here.
    expect(window.location.search).toContain(`organization=${other}`);
    expect(container.textContent).not.toContain('Consent granted');
  });

  it('ignores a check that answers after the values changed', async () => {
    let answer: (response: Response) => void = () => undefined;
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation((url: string, init?: RequestInit) =>
      String(url).includes('/onboarding/verify') ? new Promise<Response>(resolve => { answer = resolve; }) : listing(url, init));
    await render();
    act(() => button('Amazon Web Services').click());
    await submit(setInput('aws-account', '123456789012'));
    act(() => button('Verify connection').click());
    await settle();
    // Edited while the check is in flight.
    setInput('connect-key', 'bedrock-other');
    answer(json(connected));
    await settle();
    expect(container.textContent).not.toContain('Connected.');
  });

  it('ignores instructions that answer after the account changed', async () => {
    let answer: (response: Response) => void = () => undefined;
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation((url: string, init?: RequestInit) =>
      String(url).includes('/onboarding/aws?') ? new Promise<Response>(resolve => { answer = resolve; }) : listing(url, init));
    await render();
    act(() => button('Amazon Web Services').click());
    await submit(setInput('aws-account', '123456789012'));
    setInput('aws-account', '210987654321');
    answer(json({ role_arn: 'arn:aws:iam::123456789012:role/QucelerateGateway', external_id: 'x', launch_url: null, template_url: null, cli: 'old account', terraform: '' }));
    await settle();
    expect(container.querySelector('pre.script')).toBeNull();
    expect(container.textContent).not.toContain('old account');
  });

  it('saves again against the current version after a conflict', async () => {
    accounts = [{ id: 'a', release_id: 'r', adapter_instance_key: 'bedrock-main', family: 'bedrock', deployment_count: 2 }];
    let version = 3;
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (String(url).includes('/catalog/credentials') && init?.method === 'PUT') {
        if (version === 3) {
          version = 4;
          return json({ type: '', code: 'version_conflict', title: '', status: 409, detail: '', instance: '', request_id: '' }, 409);
        }
        return json({ adapter_instance_key: 'bedrock-main', family: 'bedrock', key_id: '0123456789abcdef', version: 5, created_at: '', rotated_at: '' });
      }
      if (String(url).includes('/catalog/credentials')) {
        return json({ items: [{ adapter_instance_key: 'bedrock-main', family: 'bedrock', key_id: '0123456789abcdef', version, created_at: '', rotated_at: '' }], next_page_token: null });
      }
      return listing(url, init);
    });
    await render();
    act(() => button('Amazon Web Services').click());
    await submit(setInput('aws-account', '123456789012'));
    const select = document.getElementById('connect-key') as HTMLSelectElement;
    act(() => {
      select.value = 'bedrock-main';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => button('Verify connection').click());
    await settle();
    act(() => button('Save for bedrock-main').click());
    await settle();
    expect(container.textContent).toContain('Someone changed this account’s credential meanwhile.');
    act(() => button('Save for bedrock-main').click());
    await settle();
    const puts = fetchMock.mock.calls
      .filter(([, request]) => (request as RequestInit | undefined)?.method === 'PUT')
      .map(([, request]) => new Headers((request as RequestInit).headers));
    expect(puts.map(headers => headers.get('If-Match'))).toEqual(['"v3"', '"v4"']);
    expect(puts[1].get('Idempotency-Key')).not.toBe(puts[0].get('Idempotency-Key'));
    expect(container.textContent).toContain('Saved for bedrock-main');
  });

  it('checks the Google location before asking for instructions', async () => {
    await render();
    act(() => button('Google Cloud').click());
    setInput('google-project', 'acme-ai-123');
    await submit(setInput('google-location', 'US Central'));
    expect(container.textContent).toContain('Enter global, or a region such as us-central1.');
    expect(fetchMock.mock.calls.some(([url]) => String(url).includes('/onboarding/google?'))).toBe(false);
  });

  it('asks for the Azure commands again after a failure', async () => {
    let fail = true;
    const listing = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => {
      if (String(url).includes('/onboarding/azure?resource_id=') && fail) {
        return json({ type: '', code: 'service_unavailable', title: '', status: 503, detail: 'down', instance: '', request_id: '' }, 503);
      }
      return listing(url, init);
    });
    await render();
    act(() => button('Microsoft Azure').click());
    await settle();
    const resource = '/subscriptions/00000000-0000-4000-8000-000000000001/resourceGroups/rg/providers/Microsoft.CognitiveServices/accounts/Acme';
    const input = setInput('azure-resource', resource);
    await submit(input);
    const asked = () => fetchMock.mock.calls.filter(([url]) => String(url).includes('/onboarding/azure?resource_id=')).length;
    expect(asked()).toBe(1);
    fail = false;
    await submit(input);
    expect(asked()).toBe(2);
    expect(container.querySelector('pre.script')?.textContent).toContain('az role assignment create');
  });

  const requests = (method: string, path: RegExp) => fetchMock.mock.calls
    .filter(([url, init]) => path.test(String(url)) && ((init as RequestInit | undefined)?.method ?? 'GET') === method);
  const verifyAws = async () => {
    await render();
    act(() => button('Amazon Web Services').click());
    await submit(setInput('aws-account', '123456789012'));
    act(() => button('Verify connection').click());
    await settle();
  };
  const choose = (model: string) => {
    const box = Array.from(container.querySelectorAll('.check-option')).find(node => node.textContent === model)?.querySelector('input') as HTMLInputElement;
    act(() => box.click());
  };

  it('adds a verified account to the catalog with the models, prices and aliases chosen', async () => {
    await verifyAws();
    expect(button('Add bedrock-123456789012 and activate').disabled).toBe(true);
    choose('anthropic.claude');
    setInput('add-anthropic-claude-alias', 'haiku');
    // The published price fills the form, restated per million tokens.
    act(() => button('Search').click());
    await settle();
    expect(requests('GET', /\/price-observations\?/)[0][0]).toBe(`/api/admin/v1/organizations/${ORG}/catalog/price-observations?q=anthropic.claude&page_size=20`);
    act(() => button('Use').click());
    // Tool calling is the admin's choice on Bedrock: off by default.
    expect((document.getElementById('add-anthropic-claude-tools') as HTMLInputElement).checked).toBe(false);
    expect((document.getElementById('add-anthropic-claude-input') as HTMLInputElement).value).toBe('0.25');
    expect((document.getElementById('add-anthropic-claude-output') as HTMLInputElement).value).toBe('1.25');
    expect(container.textContent).toContain('From aws-price-list:bedrock: Claude 3 Haiku');

    accounts = [{ id: 'b', release_id: 'rel-2', adapter_instance_key: 'bedrock-123456789012', family: 'bedrock', deployment_count: 1 }];
    act(() => button('Add bedrock-123456789012 and activate').click());
    await settle();
    const [[, create]] = requests('POST', /\/catalog\/releases$/);
    const manifest = JSON.parse(String((create as RequestInit).body));
    // The active release, plus the account.
    expect(manifest.providers).toEqual([...baseManifest.providers, { adapter_instance_key: 'bedrock-123456789012', family: 'bedrock' }]);
    expect(manifest.deployments[1]).toMatchObject({ name: 'bedrock-123456789012-anthropic.claude', model: 'anthropic.claude', location: 'us-east-2' });
    expect(manifest.deployments[1].capabilities[0].tools).toBeUndefined();
    expect(manifest.prices[1]).toMatchObject({ deployment: 'bedrock-123456789012-anthropic.claude', operation: 'chat', input_minor_per_token: 25, output_minor_per_token: 125, source: 'aws-price-list:bedrock', source_version: 'Claude 3 Haiku', cached_input_minor_per_token: 25, confidence: 'conservative' });
    // The list publishes no cached input price: input stands in, an upper bound.
    expect(manifest.routes.slice(1)).toEqual([
      { alias: 'haiku', operation: 'chat', deployment: 'bedrock-123456789012-anthropic.claude' },
    ]);
    // Activated at the version the active release was read at.
    const [[activateUrl, activate]] = requests('POST', /\/activate$/);
    expect(activateUrl).toBe(`/api/admin/v1/organizations/${ORG}/catalog/releases/rel-2/activate`);
    expect(new Headers((activate as RequestInit).headers).get('If-Match')).toBe('"v3"');
    expect(container.textContent).toContain('Added bedrock-123456789012 with one model');
    // Now declared, the connection can be saved for it.
    expect(button('Save for bedrock-123456789012')).toBeDefined();
  });

  it('builds on a release activated meanwhile instead of replacing it', async () => {
    await verifyAws();
    choose('anthropic.claude');
    setInput('add-anthropic-claude-input', '0.25');
    setInput('add-anthropic-claude-output', '1.25');
    activation = () => json({ type: 'about:blank', title: 'Conflict', status: 409, code: 'version_conflict', detail: 'stale' }, 409);
    act(() => button('Add bedrock-123456789012 and activate').click());
    await settle();
    expect(container.textContent).toContain('Another release was activated meanwhile');
    activation = () => json({ release: { id: 'rel-2' }, activation_version: 5 });
    act(() => button('Add bedrock-123456789012 and activate').click());
    await settle();
    // The catalog is read again and a new release requested.
    expect(requests('GET', /\/manifest$/)).toHaveLength(2);
    const keys = requests('POST', /\/catalog\/releases$/).map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(keys).toHaveLength(2);
    expect(keys[0]).not.toBe(keys[1]);
    expect(container.textContent).toContain('Added bedrock-123456789012');
  });

  it('says what to fix before anything is sent', async () => {
    await verifyAws();
    choose('anthropic.claude');
    setInput('add-anthropic-claude-alias', 'smart');
    setInput('add-anthropic-claude-input', '0.25');
    setInput('add-anthropic-claude-output', 'lots');
    expect(container.textContent).toContain('Enter the output price per million tokens');
    expect(button('Add bedrock-123456789012 and activate').disabled).toBe(true);
    setInput('add-anthropic-claude-output', '1.25');
    act(() => button('Add bedrock-123456789012 and activate').click());
    await settle();
    // `smart` already routes chat in the active release.
    expect(container.textContent).toContain('The alias smart already routes chat; choose another.');
    expect(requests('POST', /\/catalog\/releases$/)).toHaveLength(0);
  });

  it('saves the connection for the account just added even if the catalog cannot be read again', async () => {
    await verifyAws();
    choose('anthropic.claude');
    setInput('add-anthropic-claude-input', '0.25');
    setInput('add-anthropic-claude-output', '1.25');
    accountsFail = true;
    act(() => button('Add bedrock-123456789012 and activate').click());
    await settle();
    expect(container.textContent).toContain('Added bedrock-123456789012 with one model');
    act(() => button('Save for bedrock-123456789012').click());
    await settle();
    expect(requests('PUT', /\/catalog\/credentials\/bedrock-123456789012$/)).toHaveLength(1);
  });

  it('holds the form still while a release is added', async () => {
    await verifyAws();
    choose('anthropic.claude');
    setInput('add-anthropic-claude-input', '0.25');
    setInput('add-anthropic-claude-output', '1.25');
    let answer: (response: Response) => void = () => undefined;
    activation = () => new Promise<Response>(resolve => { answer = resolve; });
    act(() => button('Add bedrock-123456789012 and activate').click());
    await settle();
    const fieldset = container.querySelector('.add-provider fieldset') as HTMLFieldSetElement;
    expect(fieldset.disabled).toBe(true);
    expect(fieldset.contains(document.getElementById('add-anthropic-claude-alias'))).toBe(true);
    expect(fieldset.contains(button('Search'))).toBe(true);
    await act(async () => { answer(json({ release: { id: 'rel-2' }, activation_version: 4 })); });
    await settle();
    expect(container.textContent).toContain('Added bedrock-123456789012');
  });
  const verifyAnswers = (models: string[]) => {
    const original = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (
      url.includes('/onboarding/verify')
        ? json({ status: 'connected', message: `The gateway signed in and can see ${models.length} models.`, model_count: models.length, models: models.map(model => ({ model, display_name: null })) })
        : original(url, init)
    ));
  };
  const value = (id: string) => (document.getElementById(id) as HTMLInputElement).value;
  const addedManifest = () => JSON.parse(String((requests('POST', /\/catalog\/releases$/)[0][1] as RequestInit).body));

  it('connects an Anthropic account with an API key, checked before it is sent', async () => {
    verifyAnswers(['claude-sonnet-4-5']);
    await render();
    act(() => button('Anthropic').click());
    const input = document.getElementById('anthropic-key') as HTMLInputElement;
    expect(input.type).toBe('password');
    expect(input.required).toBe(true);
    expect(container.textContent).toContain('Stored encrypted');
    expect(container.textContent).toContain('with a spend limit');
    // Nothing to check until there is a key.
    expect(button('Verify connection')).toBeUndefined();
    setInput('anthropic-key', 'sk-ant api03');
    expect(container.textContent).toContain('Remove spaces and line breaks.');
    expect(button('Verify connection')).toBeUndefined();
    setInput('anthropic-key', 'sk-ant-admin01-abc');
    expect(container.textContent).toContain('Admin API key');
    expect(button('Verify connection')).toBeUndefined();
    setInput('anthropic-key', ' sk-ant-api03-secret ');
    await settle();
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()).toEqual([{ adapter_instance_key: 'anthropic-main', family: 'anthropic', credential: { api_key: 'sk-ant-api03-secret' } }]);
    // The key is never shown back.
    expect(container.textContent).not.toContain('sk-ant-api03-secret');
    expect(container.textContent).toContain('Connected.');

    // No price list: the price is entered, with cache reads and writes but
    // no reasoning price.
    choose('claude-sonnet-4-5');
    expect(button('Search')).toBeUndefined();
    expect(container.textContent).toContain('Anthropic and OpenAI publish no machine-readable price list: enter the model’s list price.');
    expect(value('add-claude-sonnet-4-5-cached')).toBe('');
    expect(document.getElementById('add-claude-sonnet-4-5-reasoning')).toBeNull();
    expect(container.textContent).toContain('Anthropic counts thinking as output, so there is no reasoning price.');
    setInput('add-claude-sonnet-4-5-input', '3');
    setInput('add-claude-sonnet-4-5-output', '15');
    setInput('add-claude-sonnet-4-5-cached', '0.3');
    // A five-minute cache write follows 1.25 times input.
    expect(value('add-claude-sonnet-4-5-cache-write')).toBe('3.75');
    // A chat batch, at half the price until another is entered.
    const batch = Array.from(container.querySelectorAll('.check-option')).find(node => node.textContent === 'Chat batch')?.querySelector('input') as HTMLInputElement;
    act(() => batch.click());
    expect(value('add-claude-sonnet-4-5-batch-input')).toBe('1.5');
    expect(value('add-claude-sonnet-4-5-batch-output')).toBe('7.5');
    // Tool calling, on by default for Anthropic.
    const tools = document.getElementById('add-claude-sonnet-4-5-tools') as HTMLInputElement;
    expect(tools.checked).toBe(true);
    act(() => button('Add anthropic-main and activate').click());
    await settle();
    const manifest = addedManifest();
    expect(manifest.deployments.at(-1).capabilities.map((capability: { tools?: boolean }) => capability.tools)).toEqual([true, true]);
    expect(manifest.providers.at(-1)).toEqual({ adapter_instance_key: 'anthropic-main', family: 'anthropic' });
    expect(manifest.deployments.at(-1).capabilities.map((capability: { operation: string }) => capability.operation)).toEqual(['chat', 'chat_batch']);
    expect(manifest.prices.slice(1)).toMatchObject([
      { operation: 'chat', input_minor_per_token: 300, output_minor_per_token: 1500, cached_input_minor_per_token: 30, reasoning_minor_per_token: 0, other_minor_per_token: 375, source: 'admin', confidence: 'known' },
      // Half of 3.75 per million rounds up to a whole unit per token.
      { operation: 'chat_batch', input_minor_per_token: 150, output_minor_per_token: 750, cached_input_minor_per_token: 15, reasoning_minor_per_token: 0, other_minor_per_token: 188, confidence: 'conservative' },
    ]);
    expect(manifest.routes.slice(1)).toEqual([
      { alias: 'claude-sonnet-4-5', operation: 'chat', deployment: 'anthropic-main-claude-sonnet-4-5' },
      { alias: 'claude-sonnet-4-5', operation: 'chat_batch', deployment: 'anthropic-main-claude-sonnet-4-5' },
    ]);
  });

  it('connects an OpenAI project, with reasoning charged at the output price unless priced', async () => {
    // The active release declares openai-main already.
    accounts = [{ id: 'o', release_id: 'rel-1', adapter_instance_key: 'openai-main', family: 'openai', deployment_count: 1 }];
    verifyAnswers(['gpt-5', 'text-embedding-3-small']);
    await render();
    act(() => button('OpenAI').click());
    expect(button('Verify connection')).toBeUndefined();
    setInput('openai-key', 'sk-admin-abc');
    expect(container.textContent).toContain('Admin key');
    setInput('openai-key', 'sk-proj-secret');
    // The suggested name waits for the catalog's account names.
    expect(button('Verify connection').disabled).toBe(true);
    await settle();
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()).toEqual([{ adapter_instance_key: 'openai-main-2', family: 'openai', credential: { api_key: 'sk-proj-secret' } }]);
    choose('gpt-5');
    expect(button('Search')).toBeUndefined();
    expect(container.textContent).toContain('Anthropic and OpenAI publish no machine-readable price list');
    setInput('add-gpt-5-input', '1.25');
    setInput('add-gpt-5-output', '10');
    // Reasoning is empty: zero, charged at the output price, and not tied to it.
    expect(value('add-gpt-5-reasoning')).toBe('');
    expect(container.textContent).toContain('Empty or 0: reasoning tokens are charged at the output price');
    setInput('add-gpt-5-output', '12');
    expect(value('add-gpt-5-reasoning')).toBe('');
    setInput('add-gpt-5-output', '10');
    setInput('add-gpt-5-cached', '0.125');
    // On by default for OpenAI; the admin can turn it off.
    const tools = document.getElementById('add-gpt-5-tools') as HTMLInputElement;
    expect(tools.checked).toBe(true);
    act(() => tools.click());
    act(() => button('Add openai-main-2 and activate').click());
    await settle();
    expect(addedManifest().prices.at(-1)).toMatchObject({ deployment: 'openai-main-2-gpt-5', operation: 'chat', input_minor_per_token: 125, output_minor_per_token: 1000, cached_input_minor_per_token: 13, reasoning_minor_per_token: 0, confidence: 'conservative' });
    expect(addedManifest().deployments.at(-1).capabilities[0].tools).toBeUndefined();
  });

  it('accepts an OpenAI reasoning price of its own', async () => {
    accounts = [{ id: 'o', release_id: 'rel-1', adapter_instance_key: 'openai-main', family: 'openai', deployment_count: 1 }];
    verifyAnswers(['o3']);
    await render();
    act(() => button('OpenAI').click());
    setInput('openai-key', 'sk-proj-secret');
    await settle();
    act(() => button('Verify connection').click());
    await settle();
    choose('o3');
    setInput('add-o3-input', '2');
    setInput('add-o3-output', '8');
    setInput('add-o3-reasoning', 'x');
    expect(container.textContent).toContain('Enter the reasoning price per million tokens');
    setInput('add-o3-reasoning', '9');
    act(() => button('Add openai-main-2 and activate').click());
    await settle();
    expect(addedManifest().prices.at(-1)).toMatchObject({ operation: 'chat', output_minor_per_token: 800, reasoning_minor_per_token: 900 });
  });

  it('still offers Anthropic and OpenAI when cloud onboarding cannot be loaded', async () => {
    verifyAnswers(['claude-sonnet-4-5']);
    const original = fetchMock.getMockImplementation() as (url: string, init?: RequestInit) => Promise<Response>;
    let failing = true;
    fetchMock.mockImplementation(async (url: string, init?: RequestInit) => (url.endsWith('/onboarding') && failing ? json({}, 500) : original(url, init)));
    await render();
    expect(Array.from(container.querySelectorAll('.cloud-choice')).map(node => node.querySelector('strong')?.textContent)).toEqual(['Anthropic', 'OpenAI']);
    // The failure is said where the clouds would be, not instead of the screen.
    expect(container.textContent).toContain('The cloud onboarding could not be loaded.');
    expect(container.textContent).not.toContain('Cloud accounts (AWS, Azure, Google Cloud) are offered once');
    act(() => button('Anthropic').click());
    setInput('anthropic-key', 'sk-ant-api03-secret');
    await settle();
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0]).toMatchObject({ adapter_instance_key: 'anthropic-main', family: 'anthropic' });
    // Trying again brings the clouds back.
    failing = false;
    act(() => button('Try again').click());
    await settle();
    expect(Array.from(container.querySelectorAll('.cloud-choice')).map(node => node.querySelector('strong')?.textContent)).toEqual(['Anthropic', 'OpenAI', 'Amazon Web Services', 'Microsoft Azure', 'Google Cloud']);
    expect(container.textContent).not.toContain('could not be loaded');
  });

  it('suggests a free name when the default account name is taken', async () => {
    accounts = [{ id: 'a', release_id: 'r', adapter_instance_key: 'anthropic-main', family: 'anthropic', deployment_count: 1 }];
    await render();
    act(() => button('Anthropic').click());
    setInput('anthropic-key', 'sk-ant-api03-secret');
    await settle();
    const select = document.getElementById('connect-key') as HTMLSelectElement;
    expect(Array.from(select.options).map(option => option.textContent)).toEqual(['New: anthropic-main-2', 'anthropic-main']);
    expect(select.value).toBe('');
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[0].adapter_instance_key).toBe('anthropic-main-2');
    // Choosing the existing account replaces its key.
    act(() => {
      select.value = 'anthropic-main';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    act(() => button('Verify connection').click());
    await settle();
    expect(verifyBodies()[1].adapter_instance_key).toBe('anthropic-main');
    act(() => button('Save for anthropic-main').click());
    await settle();
    const [[, init]] = requests('PUT', /\/catalog\/credentials\/anthropic-main$/);
    expect(JSON.parse(String((init as RequestInit).body))).toEqual({ api_key: 'sk-ant-api03-secret' });
  });
});
