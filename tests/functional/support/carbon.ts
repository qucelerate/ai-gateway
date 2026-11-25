import { expect, type Locator } from '@playwright/test';

/**
 * Sets a checkbox the way a person does. A Carbon checkbox hides its input
 * and draws the box on its label, so Playwright's check() would wait on an
 * input it cannot click: click the label instead. An input without an id (an
 * admin from before Carbon, as the published image the client checks run
 * against may be) is set directly.
 */
export async function setChecked(box: Locator, checked: boolean): Promise<void> {
  const id = await box.getAttribute('id');
  if (!id) {
    await box.setChecked(checked);
  } else if ((await box.isChecked()) !== checked) {
    await box.page().locator(`label[for="${id}"]`).click();
  }
  await expect(box).toBeChecked({ checked });
}
