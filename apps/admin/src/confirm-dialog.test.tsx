import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import React, { useState } from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { act } from 'react-dom/test-utils';
import { ConfirmDialog } from './ui';

describe('ConfirmDialog in-flight behavior', () => {
  let container: HTMLDivElement;
  let root: Root;

  beforeEach(() => {
    (globalThis as { IS_REACT_ACT_ENVIRONMENT?: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    container = document.createElement('div');
    document.body.appendChild(container);
    root = createRoot(container);
  });

  afterEach(() => {
    act(() => root.unmount());
    container.remove();
  });

  function TestApp() {
    const [open, setOpen] = useState(true);
    return (
      <>
        <button data-testid="trigger" onClick={() => setOpen(true)}>
          Open
        </button>
        <ConfirmDialog
          open={open}
          onClose={() => setOpen(false)}
          onConfirm={() => {}}
          title="Delete"
          confirmText="Delete"
          problem={{
            type: 'about:blank',
            code: 'version_conflict',
            title: 'Conflict',
            status: 409,
            detail: 'Someone changed this meanwhile',
            instance: '/test',
            request_id: 'req-409',
          }}
          submitting
        >
          Are you sure?
        </ConfirmDialog>
      </>
    );
  }

  it('disables both action and cancel buttons while submitting and shows the inline problem', () => {
    act(() => root.render(<TestApp />));
    const buttons = container.querySelectorAll('button');
    const cancel = Array.from(buttons).find(b => b.textContent === 'Cancel');
    const confirm = Array.from(buttons).find(b => b.textContent === 'Delete');
    expect(cancel).toBeDefined();
    expect(confirm).toBeDefined();
    expect(cancel?.disabled).toBe(true);
    expect(confirm?.disabled).toBe(true);
    expect(container.textContent).toContain('version_conflict');
    expect(container.textContent).toContain('Someone changed this meanwhile');
  });
});
