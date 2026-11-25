import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { Button } from './ui';

describe('Button', () => {
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

  it('is busy and disabled while loading, and idle otherwise', () => {
    act(() => root.render(<Button variant="ghost" type="button" loading>Load more</Button>));
    const button = container.querySelector('button')!;
    expect(button.getAttribute('aria-busy')).toBe('true');
    expect(button.disabled).toBe(true);
    expect(button.textContent).toBe('Load more');
    act(() => root.render(<Button variant="ghost" type="button" loading={false}>Load more</Button>));
    expect(button.hasAttribute('aria-busy')).toBe(false);
    expect(button.disabled).toBe(false);
  });

  it('keeps a caller’s own aria-busy, and submits inside a form by default', () => {
    act(() => root.render(<form><Button aria-busy>Save</Button></form>));
    const button = container.querySelector('button')!;
    expect(button.getAttribute('aria-busy')).toBe('true');
    expect(button.type).toBe('submit');
  });
});
