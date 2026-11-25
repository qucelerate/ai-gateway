import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { act } from 'react-dom/test-utils';
import { createRoot, type Root } from 'react-dom/client';
import { TabPanel, Tabs, routeTab, useRouteTab } from './ui';

const keys = ['one', 'two', 'three'] as const;
type Key = typeof keys[number];
const tabs = keys.map(key => [key, key.toUpperCase()] as const);

function Screen() {
  const [tab, setTab] = useRouteTab<Key>(keys, 'one');
  return (
    <>
      <Tabs id="t" label="Sections" tabs={tabs} value={tab} onChange={setTab} />
      <TabPanel id="t" value={tab}>panel {tab}</TabPanel>
    </>
  );
}

describe('tabs kept in the path', () => {
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
    history.replaceState(null, '', '/');
  });
  const tab = (name: string) => container.querySelector<HTMLButtonElement>(`[role="tab"][id="t-tab-${name}"]`)!;
  const panel = () => container.querySelector<HTMLElement>('[role="tabpanel"]')!;
  const key = (target: Element, name: string) => act(() => { target.dispatchEvent(new KeyboardEvent('keydown', { key: name, bubbles: true })); });

  it('opens the tab the path names, and the fallback for none or an unknown one', () => {
    history.replaceState(null, '', '/admin/page/two?x=1');
    act(() => root.render(<Screen />));
    expect(panel().textContent).toBe('panel two');
    expect(panel().getAttribute('aria-labelledby')).toBe('t-tab-two');
    act(() => root.unmount());
    root = createRoot(container);
    history.replaceState(null, '', '/admin/page/nope');
    act(() => root.render(<Screen />));
    expect(panel().textContent).toBe('panel one');
  });

  it('puts the chosen tab in the path, keeping the query, and follows Back', () => {
    history.replaceState(null, '', '/admin/page?x=1');
    act(() => root.render(<Screen />));
    act(() => tab('three').click());
    expect(location.pathname).toBe('/admin/page/three');
    expect(location.search).toBe('?x=1');
    expect(routeTab()).toBe('three');
    history.replaceState(null, '', '/admin/page/two');
    act(() => { dispatchEvent(new PopStateEvent('popstate')); });
    expect(panel().textContent).toBe('panel two');
  });

  it('follows the shell going to the page already open, which names no tab', () => {
    history.replaceState(null, '', '/admin/page/three');
    act(() => root.render(<Screen />));
    expect(panel().textContent).toBe('panel three');
    // What the shell's navigation does.
    act(() => { history.pushState({}, '', '/admin/page'); dispatchEvent(new PopStateEvent('popstate')); });
    expect(panel().textContent).toBe('panel one');
    expect(panel().tabIndex).toBe(0);
  });

  it('is one tab stop; arrows, Home and End move between tabs and open them', () => {
    history.replaceState(null, '', '/admin/page');
    act(() => root.render(<Screen />));
    expect(tab('one').tabIndex).toBe(0);
    expect(tab('two').tabIndex).toBe(-1);
    key(tab('one'), 'ArrowRight');
    expect(tab('two').getAttribute('aria-selected')).toBe('true');
    expect(document.activeElement).toBe(tab('two'));
    key(tab('two'), 'End');
    expect(document.activeElement).toBe(tab('three'));
    key(tab('three'), 'ArrowRight');
    expect(document.activeElement).toBe(tab('one'));
    key(tab('one'), 'ArrowLeft');
    expect(panel().textContent).toBe('panel three');
    key(tab('three'), 'Home');
    expect(location.pathname).toBe('/admin/page/one');
  });
});
