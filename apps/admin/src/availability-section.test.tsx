import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ProvidersScreen } from './providers-screen';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': 'application/json' } });
const CATALOG = '/api/admin/v1/organizations/org-1/catalog';
const bedrock = { id: 'a-1', release_id: 'r-1', adapter_instance_key: 'bedrock-1', family: 'bedrock', deployment_count: 1 };
const foundry = { id: 'a-2', release_id: 'r-1', adapter_instance_key: 'foundry', family: 'azure-ai-foundry', deployment_count: 1 };
const evidence = (state: string, reason: string | null = null) => ({
  state, reason, source: 'probe', deployment: null, provider_status: null, provider_code: null, observed_at: '2026-11-05T00:00:00Z', verified_at: null,
});
const model = (overrides: Record<string, unknown>) => ({
  adapter_instance_key: 'bedrock-1', provider_family: 'bedrock', model: 'amazon.nova-pro-v1:0', display_name: null,
  first_seen_at: '2026-10-10T00:00:00Z', last_seen_at: '2026-11-05T00:00:00Z', missing_since: null, deployed: true,
  operations: null, availability: 'listed', evidence: evidence('verified'), deployment: null, ...overrides,
});
const blocked = model({ model: 'anthropic.claude-opus-4-6', deployed: false, evidence: evidence('requires_action', 'agreement_required') });
const azureDeployment = model({
  adapter_instance_key: 'foundry', provider_family: 'azure-ai-foundry', model: 'gpt-5-4-prod', evidence: evidence('requires_action', 'permission_missing'),
  deployment: { format: 'OpenAI', model: 'gpt-5.4', version: '2026-03-01', sku: 'GlobalStandard', capacity: 50 },
});
const deployable = (overrides: Record<string, unknown>) => ({
  adapter_instance_key: 'foundry', provider_family: 'azure-ai-foundry', format: 'OpenAI', model: 'gpt-5.5', version: '2026-09-01',
  lifecycle_status: 'GenerallyAvailable', default_version: true, skus: ['GlobalStandard'], max_capacity: 1000, retires_at: null,
  operations: ['chat', 'chat_stream'], deployments: [], listed_at: '2026-11-05T00:00:00Z', ...overrides,
});

describe('availability tab', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let failRetirements: boolean;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    failRetirements = false;
    fetchMock = vi.fn(async (url: string) => {
      if (url.startsWith(`${CATALOG}/provider-accounts`)) return json({ items: [bedrock, foundry], next_page_token: null, release_id: 'r-1' });
      if (url.startsWith(`${CATALOG}/deployments`)) {
        return json({ items: [{ id: 'd-1', provider_account_id: 'a-1', adapter_instance_key: 'bedrock-1', family: 'bedrock', name: 'nova', model: 'amazon.nova-pro-v1:0', location: 'us-east-2', operations: [] }], next_page_token: null, release_id: 'r-1' });
      }
      if (url.startsWith(`${CATALOG}/price-drift`)) return json({ release_id: 'r-1', prices: [] });
      if (url.startsWith(`${CATALOG}/discovered-models`)) {
        const query = new URL(url, 'http://x').searchParams;
        if (query.get('adapter_instance_key') === 'foundry') return json({ items: [azureDeployment], next_page_token: null });
        const states = (query.get('evidence') ?? '').split(',');
        return json({ items: [model({}), blocked].filter(item => states.includes(item.evidence.state)), next_page_token: null });
      }
      if (url.startsWith(`${CATALOG}/deployable-models`)) {
        const query = new URL(url, 'http://x').searchParams;
        if (query.get('deployed') === 'true' && failRetirements) return json({ type: 'about:blank', code: 'unavailable', title: 'x', status: 503, detail: '', instance: '', request_id: 'r' }, 503);
        return query.get('deployed') === 'true'
          ? json({
            items: [
              deployable({ model: 'gpt-5.4', deployments: ['gpt-5-4-prod'], retires_at: '2027-02-01T00:00:00Z' }),
              // An old family's model under the same key and deployment name.
              deployable({ provider_family: 'azure-openai', deployments: ['gpt-5-4-prod'], retires_at: '2026-01-01T00:00:00Z' }),
            ],
            next_page_token: null,
          })
          : json({ items: [deployable({})], next_page_token: null });
      }
      return json({ items: [], next_page_token: null, release_id: 'r-1' });
    });
    vi.stubGlobal('fetch', fetchMock);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.unstubAllGlobals();
  });

  const settle = async () => {
    for (let i = 0; i < 10; i += 1) await act(async () => { await Promise.resolve(); });
  };
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent === text) as HTMLButtonElement;
  const click = async (target: HTMLElement) => {
    act(() => target.click());
    await settle();
  };
  const choose = async (id: string, value: string) => {
    const select = document.getElementById(id) as HTMLSelectElement;
    act(() => {
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
  };
  const urls = (fragment: string) => fetchMock.mock.calls.map(([url]) => String(url)).filter(url => url.includes(fragment));
  const open = async () => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" role="Viewer" />));
    await settle();
    openSection('models', 'availability');
    await settle();
  };

  it('shows what a serverless account can call, and what blocks the rest', async () => {
    await open();
    const deployedUrl = decodeURIComponent(urls('/discovered-models').pop()!);
    expect(deployedUrl).toContain('state=listed');
    expect(deployedUrl).toContain('evidence=verified,entitled,none');
    const deployed = container.querySelector('[aria-label="Deployed models of bedrock-1"]')!;
    expect(deployed.textContent).toContain('amazon.nova-pro-v1:0');
    expect(deployed.textContent).toContain('Verified');
    expect(deployed.textContent).not.toContain('claude-opus');

    await click(button('Deployable'));
    expect(decodeURIComponent(urls('/discovered-models').pop()!)).toContain('evidence=requires_action,refused');
    const table = container.querySelector('[aria-label="Deployable models of bedrock-1"]')!;
    expect(table.textContent).toContain('anthropic.claude-opus-4-6');
    expect(table.textContent).toContain('The model’s offer terms are not accepted.');
    const link = table.querySelector('a')!;
    expect(link.getAttribute('href')).toBe('https://us-east-2.console.aws.amazon.com/bedrock/home?region=us-east-2#/model-catalog');
    expect(link.getAttribute('rel')).toBe('noopener noreferrer');
    // Accepting terms may not be enough: some models need a request to AWS.
    expect(table.textContent).toContain('need access AWS grants on request');
    expect(Array.from(table.querySelectorAll('a')).some(anchor => anchor.getAttribute('href')?.endsWith('/guide/providers#model-access-on-your-cloud-account'))).toBe(true);
  });

  it('shows every Azure deployment with its reason, and the catalog models without one', async () => {
    await open();
    await choose('availability-account', 'foundry');
    expect(decodeURIComponent(urls('/discovered-models').pop()!)).not.toContain('evidence=');
    const deployed = container.querySelector('[aria-label="Deployed models of foundry"]')!;
    expect(deployed.textContent).toContain('gpt-5-4-prod');
    expect(deployed.textContent).toContain('OpenAI · gpt-5.4 2026-03-01');
    expect(deployed.textContent).toContain('GlobalStandard, capacity 50');
    expect(deployed.textContent).toContain('Needs action');
    expect(deployed.textContent).toContain('Cognitive Services OpenAI User');
    expect(deployed.textContent).toContain(`Retires ${new Date('2027-02-01T00:00:00Z').toLocaleString()}`);
    expect(deployed.querySelector('a')?.getAttribute('href')).toBe('https://portal.azure.com/');

    await click(button('Deployable'));
    expect(urls('/deployable-models').pop()).toContain('deployed=false');
    const table = container.querySelector('[aria-label="Deployable models of foundry"]')!;
    expect(table.textContent).toContain('gpt-5.5');
    expect(table.textContent).toContain('Chat—');
    expect(table.textContent).not.toContain('Chat, Chat');
    expect(container.textContent).toContain('Create a deployment of it in the Azure AI Foundry portal');

    await choose('deployable-operation', 'embeddings');
    expect(urls('/deployable-models').pop()).toContain('operation=embeddings');
  });

  it('reports retirement dates that could not be loaded, and loads them again on Refresh', async () => {
    failRetirements = true;
    await open();
    await choose('availability-account', 'foundry');
    expect(container.textContent).toContain('retirement dates');
    const before = urls('deployed=true').length;
    failRetirements = false;
    await click(button('Refresh'));
    expect(urls('deployed=true').length).toBeGreaterThan(before);
    expect(container.querySelector('[aria-label="Deployed models of foundry"]')!.textContent).toContain('Retires');
  });
});
