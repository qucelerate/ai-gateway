import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import React, { useState } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ApiProblemError, type ApiProblem } from './api';
import { useMutationState, useSecretReveal } from './mutation-state';

describe('mutation state behavior', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
    localStorage.clear();
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
    vi.restoreAllMocks();
  });

  function MutationTestComponent() {
    const [value, setValue] = useState('kept');
    const mutation = useMutationState<string>();
    return (
      <div>
        <input
          data-testid="input"
          value={value}
          onChange={event => setValue(event.target.value)}
        />
        <button
          data-testid="submit"
          onClick={() => {
            void mutation.submit(async () => ({ data: 'ok', requestId: 'req-1' }));
          }}
        >
          Submit
        </button>
        <button data-testid="change" onClick={() => setValue('changed')}>
          Change
        </button>
        <button
          data-testid="fail"
          onClick={() => {
            const problem: ApiProblem = {
              type: 'about:blank',
              code: 'version_conflict',
              title: 'Conflict',
              status: 409,
              detail: 'resource version conflict; current version is 2',
              instance: '/teams/team-1',
              request_id: 'req-409',
            };
            void mutation.submit(async () => {
              throw new ApiProblemError(409, problem);
            });
          }}
        >
          Fail
        </button>
        <div data-testid="submitting">{mutation.submitting ? 'yes' : 'no'}</div>
        <div data-testid="problem">{mutation.problem?.code ?? 'none'}</div>
        <div data-testid="result">{mutation.result?.data ?? 'none'}</div>
      </div>
    );
  }

  it('tracks successful mutations and shows the request reference', async () => {
    act(() => root.render(<MutationTestComponent />));
    const submit = container.querySelector('[data-testid="submit"]') as HTMLButtonElement;
    await act(async () => {
      submit.click();
    });
    expect(container.querySelector('[data-testid="submitting"]')?.textContent).toBe('no');
    expect(container.querySelector('[data-testid="result"]')?.textContent).toBe('ok');
    expect(container.querySelector('[data-testid="problem"]')?.textContent).toBe('none');
  });

  it('preserves form input on conflict errors and exposes the server explanation', async () => {
    act(() => root.render(<MutationTestComponent />));
    const change = container.querySelector('[data-testid="change"]') as HTMLButtonElement;
    const fail = container.querySelector('[data-testid="fail"]') as HTMLButtonElement;
    const input = container.querySelector('[data-testid="input"]') as HTMLInputElement;
    await act(async () => {
      change.click();
    });
    await act(async () => {
      fail.click();
    });
    expect(container.querySelector('[data-testid="submitting"]')?.textContent).toBe('no');
    expect(container.querySelector('[data-testid="problem"]')?.textContent).toBe('version_conflict');
    expect(container.querySelector('[data-testid="result"]')?.textContent).toBe('none');
    expect(input.value).toBe('changed');
  });

  function SecretTestComponent() {
    const { secret, reveal, dismiss } = useSecretReveal();
    return (
      <div>
        <button data-testid="reveal" onClick={() => reveal('gw_super_secret', 'New key', 'req-secret')}>
          Reveal
        </button>
        <button data-testid="dismiss" onClick={dismiss}>
          Dismiss
        </button>
        <div data-testid="secret">{secret?.secret ?? 'none'}</div>
      </div>
    );
  }

  it('reveals a secret once and clears it on dismissal; it is not persisted to localStorage', async () => {
    act(() => root.render(<SecretTestComponent />));
    const reveal = container.querySelector('[data-testid="reveal"]') as HTMLButtonElement;
    const dismiss = container.querySelector('[data-testid="dismiss"]') as HTMLButtonElement;
    await act(async () => {
      reveal.click();
    });
    expect(container.querySelector('[data-testid="secret"]')?.textContent).toBe('gw_super_secret');
    expect(localStorage.getItem('gateway.admin.apiKeySecret')).toBeNull();
    await act(async () => {
      dismiss.click();
    });
    expect(container.querySelector('[data-testid="secret"]')?.textContent).toBe('none');
  });
});
