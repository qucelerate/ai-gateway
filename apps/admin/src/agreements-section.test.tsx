import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ProvidersScreen } from './providers-screen';
import { openSection } from './test-route';

const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status, headers: { 'Content-Type': status >= 400 ? 'application/problem+json' : 'application/json' } });
const CATALOG = '/api/admin/v1/organizations/org-1/catalog';
const AGREEMENTS = `${CATALOG}/agreements/bedrock-1`;
const bedrock = { id: 'a-1', release_id: 'r-1', adapter_instance_key: 'bedrock-1', family: 'bedrock', deployment_count: 1 };
const foundry = { id: 'a-2', release_id: 'r-1', adapter_instance_key: 'foundry', family: 'azure-ai-foundry', deployment_count: 1 };
const offer = (id: string) => ({
  offer_id: id,
  rate_card: [{ dimension: 'USE2_input_tokens', price: '0.000001', unit: 'Units', description: null }],
  rate_card_region: 'USE2',
  rate_card_total: 570,
  legal_url: 'https://example.invalid/terms',
  refund_policy: 'No refunds',
  agreement_duration: null,
});
const view = (models: unknown[], permission = 'granted') => ({ adapter_instance_key: 'bedrock-1', family: 'bedrock', permission, models });
const haiku = {
  model: 'anthropic.claude-haiku-5-5', unblocks: ['us.anthropic.claude-haiku-5-5', 'global.anthropic.claude-haiku-5-5'], deployments: [],
  agreement_required: true, offers: [offer('offer-haiku')], offers_error: null, agreement: null,
};
const grok = {
  model: 'xai.grok-9', unblocks: ['xai.grok-9'], deployments: ['grok'], agreement_required: true, offers: [offer('offer-grok')], offers_error: null,
  agreement: {
    status: 'refused', reason: 'access_request_required', detail: 'The agreement is in place but AWS refused the call: request access from AWS.',
    provider_code: 'accessDeniedException', offer_id: 'offer-grok', accepted_by: 'u-1', accepted_at: '2026-11-19T00:00:00Z', checked_at: '2026-11-19T00:01:00Z',
    verified_at: null, probe: true, probed_model: 'xai.grok-9', checking: false,
  },
};

describe('model agreements panel', () => {
  let container: HTMLDivElement;
  let root: Root;
  let fetchMock: ReturnType<typeof vi.fn>;
  let agreements: unknown;
  let acceptAnswer: () => Response;
  let autoOffer: Response | null;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    agreements = view([haiku, grok]);
    acceptAnswer = () => json({
      adapter_instance_key: 'bedrock-1',
      results: [{ model: 'anthropic.claude-haiku-5-5', offer_id: 'offer-haiku', outcome: 'accepted', status: 'pending', reason: null, detail: null, provider_code: null }],
    });
    autoOffer = null;
    fetchMock = vi.fn(async (url: string, init?: RequestInit) => {
      if (url === `${CATALOG}/auto-offers/bedrock-1`) {
        return autoOffer ?? json({ type: 'x', code: 'auto_offer_not_found', title: 'Not found', status: 404, detail: 'none', instance: '', request_id: 'r' }, 404);
      }
      if (url.startsWith(`${CATALOG}/provider-accounts`)) return json({ items: [bedrock, foundry], next_page_token: null, release_id: 'r-1' });
      if (url === `${AGREEMENTS}/accept` && init?.method === 'POST') return acceptAnswer();
      if (url === AGREEMENTS) return json(agreements);
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
  const button = (text: string) => Array.from(document.querySelectorAll('button')).find(node => node.textContent?.startsWith(text)) as HTMLButtonElement | undefined;
  const click = async (target: HTMLElement) => {
    act(() => target.click());
    await settle();
  };
  const posts = () => fetchMock.mock.calls.filter(([, init]) => (init as RequestInit | undefined)?.method === 'POST');
  const open = async (role: string) => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" role={role} />));
    await settle();
    openSection('models', 'availability');
    await settle();
    await click(button('Agreements')!);
  };

  it('lists the models with their offers and accepts the picked ones only after a warning', async () => {
    await open('Owner');
    const table = container.querySelector('[aria-label="Model agreements of bedrock-1"]')!;
    expect(table.textContent).toContain('anthropic.claude-haiku-5-5');
    expect(table.textContent).toContain('Unblocks us.anthropic.claude-haiku-5-5, global.anthropic.claude-haiku-5-5');
    // Each rate as AWS gives it, with whose rates they are.
    expect(table.textContent).toContain('USE2_input_tokens: 0.000001 Units (USE2’s rates)');
    expect(table.textContent).toContain('Refunds: No refunds');
    // A refusal after acceptance points at asking AWS for access.
    expect(table.textContent).toContain('Refused');
    expect(table.textContent).toContain('need access AWS grants on request');
    expect(container.textContent).toContain('Accepting does not help when AWS has not enabled the model');
    // Already accepted (refused): nothing to pick.
    expect(table.querySelector('[aria-label="Accept xai.grok-9"]')).toBeNull();

    expect(button('Accept selected')!.disabled).toBe(true);
    const pick = table.querySelector('[aria-label="Accept anthropic.claude-haiku-5-5"]') as HTMLInputElement;
    await click(pick);
    await click(button('Accept selected')!);
    const dialog = document.querySelector('[role="alertdialog"]')!;
    expect(dialog.textContent).toContain('This creates AWS Marketplace subscriptions');
    expect(dialog.textContent).toContain('billed per use at the rates shown');
    expect(dialog.textContent).toContain('anthropic.claude-haiku-5-5 with offer offer-haiku');
    const probe = Array.from(dialog.querySelectorAll('input[type="checkbox"]')).find(node => node.closest('.check-option')?.textContent?.includes('1-token test request')) as HTMLInputElement;
    expect(probe.checked).toBe(false);
    // Automatic offers are off for the account: leaving it off calls nothing.
    expect(dialog.textContent).toContain('Automatic offers are off for this account, so nothing calls the model');
    expect(posts()).toHaveLength(0);

    await click(button('Accept and subscribe')!);
    expect(posts()).toHaveLength(1);
    const [url, init] = posts()[0] as [string, RequestInit];
    expect(url).toBe(`${AGREEMENTS}/accept`);
    expect(JSON.parse(String(init.body))).toEqual({ models: [{ model: 'anthropic.claude-haiku-5-5', offer_id: 'offer-haiku' }], verify: false });
    expect(new Headers(init.headers).get('Idempotency-Key')).toBeTruthy();
    const results = container.querySelector('section[aria-label="Acceptance results"]')!;
    expect(results.textContent).toContain('Accepted');
    expect(results.textContent).toContain('Pending with AWS');
  });

  const choose = (model: string, value: string) => act(() => {
    const select = container.querySelector(`[aria-label="Offer for ${model}"]`) as HTMLSelectElement;
    select.value = value;
    select.dispatchEvent(new Event('change', { bubbles: true }));
  });
  const offerSelect = (model: string) => container.querySelector(`[aria-label="Offer for ${model}"]`) as HTMLSelectElement;
  const gets = () => fetchMock.mock.calls.filter(([url, init]) => url === AGREEMENTS && !(init as RequestInit | undefined)?.method).length;
  const llama = (...offers: string[]) => ({
    model: 'meta.llama-9', unblocks: ['meta.llama-9'], deployments: [], agreement_required: true, offers: offers.map(offer), offers_error: null, agreement: null,
  });

  it('sends the billed test request only when asked, and closes on a changed offer, dropping choices no longer offered', async () => {
    agreements = view([haiku, llama('o-1', 'o-2')]);
    acceptAnswer = () => {
      agreements = view([haiku, llama('o-3', 'o-4')]);
      return json({
        type: 'x', code: 'agreement_offer_changed', title: 'Conflict', status: 409, detail: 'changed', instance: '', request_id: 'r',
        changes: [{ model: 'meta.llama-9', requested_offer_id: 'o-2', current_offer_ids: ['o-3', 'o-4'] }],
      }, 409);
    };
    await open('Owner');
    // Several offers: Accept all leaves the model out until one is chosen.
    expect(button('Accept all')!.textContent).toBe('Accept all (1)');
    expect(offerSelect('meta.llama-9').value).toBe('');
    expect(container.textContent).toContain('One model has several offers');
    choose('meta.llama-9', 'o-2');
    await settle();
    expect(button('Accept all')!.textContent).toBe('Accept all (2)');
    await click(button('Accept all')!);
    const dialog = document.querySelector('[role="alertdialog"]')!;
    expect(dialog.textContent).toContain('Accept 2 agreements');
    expect(dialog.textContent).toContain('meta.llama-9 with offer o-2');
    const probe = Array.from(dialog.querySelectorAll('input[type="checkbox"]'))[0] as HTMLInputElement;
    await click(probe);
    const before = gets();
    await click(button('Accept and subscribe')!);
    const [, init] = posts()[0] as [string, RequestInit];
    // Exactly the pairs shown: there is no "all".
    expect(JSON.parse(String(init.body))).toEqual({
      models: [{ model: 'anthropic.claude-haiku-5-5', offer_id: 'offer-haiku' }, { model: 'meta.llama-9', offer_id: 'o-2' }],
      verify: true,
    });
    expect(document.querySelector('[role="alertdialog"]')).toBeNull();
    expect(document.body.textContent).toContain('offer changed since this page loaded, so nothing was accepted');
    expect(gets()).toBe(before + 1);
    // The reloaded model no longer offers o-2: nothing is chosen for it.
    expect(offerSelect('meta.llama-9').value).toBe('');
    expect(button('Accept all')!.textContent).toBe('Accept all (1)');
  });

  it('says what the usual check sends when automatic offers are on', async () => {
    autoOffer = json({ adapter_instance_key: 'bedrock-1', location: null, created_at: '2026-11-19T00:00:00Z', updated_at: '2026-11-19T00:00:00Z', applied_at: null, applied_release_id: null, error_code: null, version: 1, models: [] });
    await open('Owner');
    await click(container.querySelector('[aria-label="Accept anthropic.claude-haiku-5-5"]') as HTMLInputElement);
    await click(button('Accept selected')!);
    const dialog = document.querySelector('[role="alertdialog"]')!;
    expect(dialog.textContent).toContain('Automatic offers are on for this account');
    expect(dialog.textContent).toContain('one 1-token request (billed by AWS) per model identifier it would offer');
  });

  it('enables Accept selected only for picks with an offer to send', async () => {
    agreements = view([llama('o-1', 'o-2')]);
    await open('Owner');
    await click(container.querySelector('[aria-label="Accept meta.llama-9"]') as HTMLInputElement);
    expect(button('Accept selected')!.disabled).toBe(true);
    expect(container.textContent).toContain('2 offers: choose one to accept this model');
    choose('meta.llama-9', 'o-1');
    await settle();
    expect(button('Accept selected')!.disabled).toBe(false);
  });

  it('retries with the same key after a lost answer or one still in progress', async () => {
    let answers = [
      () => { throw new TypeError('network down'); },
      () => json({ type: 'x', code: 'idempotency_in_progress', title: 'Conflict', status: 409, detail: 'in progress', instance: '', request_id: 'r' }, 409),
    ];
    const accepted = acceptAnswer;
    acceptAnswer = () => (answers.shift() ?? accepted)();
    await open('Owner');
    await click(container.querySelector('[aria-label="Accept anthropic.claude-haiku-5-5"]') as HTMLInputElement);
    await click(button('Accept selected')!);
    for (let i = 0; i < 3; i += 1) await click(button('Accept and subscribe')!);
    const keys = posts().map(([, init]) => new Headers((init as RequestInit).headers).get('Idempotency-Key'));
    expect(keys).toHaveLength(3);
    expect(new Set(keys).size).toBe(1);
    expect(container.querySelector('section[aria-label="Acceptance results"]')!.textContent).toContain('Accepted');
    answers = [];
  });

  it('polls while checking, and a failed poll keeps the results shown', async () => {
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] });
    try {
      const checking = { ...haiku, agreement: { ...grok.agreement, status: 'pending', reason: null, detail: null, provider_code: null, offer_id: 'offer-haiku', probe: false, probed_model: null, checking: true } };
      await open('Owner');
      await click(container.querySelector('[aria-label="Accept anthropic.claude-haiku-5-5"]') as HTMLInputElement);
      await click(button('Accept selected')!);
      agreements = view([checking, grok]);
      await click(button('Accept and subscribe')!);
      expect(container.textContent).toContain('checking…');
      const before = gets();
      let fail = true;
      fetchMock.mockImplementation(async (url: string) => {
        if (url === AGREEMENTS && fail) return json({ type: 'x', code: 'service_unavailable', title: 'Unavailable', status: 503, detail: 'down', instance: '', request_id: 'r' }, 503);
        if (url === AGREEMENTS) return json(view([{ ...checking, agreement: { ...checking.agreement, status: 'available', reason: 'not_verified', checking: false } }, grok]));
        return json({ items: [], next_page_token: null, release_id: 'r-1' });
      });
      act(() => { vi.advanceTimersByTime(20_000); });
      await settle();
      expect(gets()).toBe(before + 1);
      // The results and the table stay, with the error beside them.
      expect(container.querySelector('section[aria-label="Acceptance results"]')).not.toBeNull();
      expect(container.querySelector('[aria-label="Model agreements of bedrock-1"]')).not.toBeNull();
      fail = false;
      act(() => { vi.advanceTimersByTime(20_000); });
      await settle();
      expect(gets()).toBe(before + 2);
      expect(container.textContent).toContain('Available, not yet called');
      expect(container.querySelector('section[aria-label="Acceptance results"]')).not.toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it('explains that an account on the operator’s credential has no agreements here', async () => {
    agreements = null;
    fetchMock.mockImplementation(async (url: string) => {
      if (url.startsWith(`${CATALOG}/provider-accounts`)) return json({ items: [bedrock, foundry], next_page_token: null, release_id: 'r-1' });
      if (url === AGREEMENTS) return json({ type: 'x', code: 'agreements_need_tenant_credential', title: 'Unprocessable', status: 422, detail: 'own role', instance: '', request_id: 'r' }, 422);
      return json({ items: [], next_page_token: null, release_id: 'r-1' });
    });
    await open('Owner');
    expect(container.textContent).toContain('Agreements need the organization’s own AWS role');
    expect(button('Accept selected')).toBeUndefined();
    expect(button('Accept all')).toBeUndefined();
  });

  it('offers no accepting to a read-only role, and explains a missing permission', async () => {
    agreements = view([haiku], 'missing');
    await open('auditor');
    expect(button('Accept selected')).toBeUndefined();
    expect(container.textContent).toContain('lacks the opt-in agreement permissions');
  });

  it('is only offered for Bedrock accounts', async () => {
    act(() => root.render(<ProvidersScreen organizationId="org-1" role="Owner" />));
    await settle();
    openSection('models', 'availability');
    await settle();
    const select = document.getElementById('availability-account') as HTMLSelectElement;
    act(() => {
      select.value = 'foundry';
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    await settle();
    expect(button('Agreements')).toBeUndefined();
  });

  it('opens a Bedrock account on Deployed after another provider’s account', async () => {
    await open('Owner');
    expect(container.querySelector('[aria-label="Model agreements of bedrock-1"]')).not.toBeNull();
    const pickAccount = (value: string) => act(() => {
      const select = document.getElementById('availability-account') as HTMLSelectElement;
      select.value = value;
      select.dispatchEvent(new Event('change', { bubbles: true }));
    });
    pickAccount('foundry');
    await settle();
    pickAccount('bedrock-1');
    await settle();
    expect(container.querySelector('[aria-label="Model agreements of bedrock-1"]')).toBeNull();
  });
});
