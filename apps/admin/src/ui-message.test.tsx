import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { FormField, Message } from './ui';

describe('Message', () => {
  let container: HTMLDivElement;
  let root: Root;
  beforeEach(() => {
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });
  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  it('alerts for errors and announces politely otherwise', () => {
    act(() => root.render(<><Message kind="error">Failed</Message><Message kind="success">Saved</Message><Message>Loading</Message></>));
    const [error, success, info] = Array.from(container.querySelectorAll('.message'));
    expect(error.getAttribute('role')).toBe('alert');
    expect(error.className).toContain('cds--inline-notification--error');
    expect(success.getAttribute('role')).toBe('status');
    expect(info.className).toContain('cds--inline-notification--info');
    expect(info.textContent).toBe('Loading');
  });

  it('shows a title above the text', () => {
    act(() => root.render(<Message kind="warning" title="Heads up">Details</Message>));
    expect(container.querySelector('.cds--inline-notification__title')?.textContent).toBe('Heads up');
    expect(container.querySelector('.cds--inline-notification__subtitle')?.textContent).toBe('Details');
  });
});

describe('FormField', () => {
  it('shows an error for a child that is not a Carbon field, outside the class Carbon hides', () => {
    const container = document.createElement('div');
    const root = createRoot(container);
    act(() => root.render(<FormField label="Range" htmlFor="range" error="Pick an end after the start"><div id="range" /></FormField>));
    const error = container.querySelector('.field-error')!;
    expect(error.textContent).toBe('Pick an end after the start');
    expect(error.classList.contains('cds--form-requirement')).toBe(false);
    act(() => root.unmount());
  });
});
