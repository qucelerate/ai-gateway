import { act } from 'react-dom/test-utils';

/** Opens a section of a page as the side navigation does: the path changes
 * and the open screen follows it on `popstate`. */
export function openSection(page: string, section: string) {
  act(() => {
    history.pushState(null, '', `/admin/${page}/${section}${location.search}`);
    dispatchEvent(new PopStateEvent('popstate'));
  });
}
