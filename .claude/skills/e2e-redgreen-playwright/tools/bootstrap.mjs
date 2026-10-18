// OpenChamber web app bootstrap for scenario scripts: handle the boot
// "Add project directory" dialog (fresh profile), ensure the scratch project
// is added and selected, open a new draft session, expose the composer.
// App-specific pitfalls (why each fallback exists) are canonically documented
// in the ompchamber-web-e2e-playwright skill; this file is their embodiment.
import os from 'node:os';
import { SCRATCH, APP_URL } from './harness.mjs';

export async function bootstrapApp(page, { scratch = SCRATCH } = {}) {
  await page.goto(APP_URL, { timeout: 60000, waitUntil: 'load' });

  // Wait for either the Add-project dialog (fresh profile) or the shell.
  const dialog = page.locator('[role=dialog]', { hasText: 'Add project directory' }).first();
  let dialogAppeared = false;
  try {
    await dialog.waitFor({ state: 'visible', timeout: 12000 });
    dialogAppeared = true;
  } catch {
    dialogAppeared = false;
  }

  if (dialogAppeared) {
    const home = os.homedir();
    if (!scratch.startsWith(home + '/')) {
      throw new Error(`scratch must live under ${home} — the Add-project dialog tree starts there (got ${scratch})`);
    }
    const segs = scratch.slice(home.length + 1).split('/');
    for (const seg of segs) {
      const row = page.getByRole('option', { name: seg, exact: true }).first();
      if (await row.count()) {
        await row.click({ timeout: 8000 });
      } else {
        await page.evaluate((name) => {
          const spans = [...document.querySelectorAll('span')].filter((s) => s.textContent.trim() === name);
          const el = spans[0]?.closest('[role=option]') ?? spans[0];
          if (!el) throw new Error('no row for ' + name);
          el.dispatchEvent(new MouseEvent('click', { bubbles: true }));
        }, seg);
      }
      await page.waitForTimeout(900);
    }
    const add = page.getByRole('button', { name: 'add project' });
    if (await add.count()) {
      await add.click({ timeout: 8000 });
      await page.waitForTimeout(2000);
    }
    // Dialog may stay open when the project already exists — close it.
    // "add project" is a no-op for an already-added directory, and Escape
    // alone does not always dismiss the base-ui dialog: loop Escape + the
    // dedicated close slot until it is gone.
    for (let i = 0; i < 5 && (await dialog.count()); i++) {
      await page.keyboard.press('Escape').catch(() => {});
      const closeBtn = page.locator('[data-slot=dialog-close]').last();
      if (await closeBtn.count()) await closeBtn.click({ timeout: 3000 }).catch(() => {});
      await page.waitForTimeout(700);
    }
    await dialog.waitFor({ state: 'hidden', timeout: 8000 }).catch(() => {});
    await page.waitForTimeout(1500);
  }

  // New session draft (sidebar nav button, not the hover "+" on rows).
  const navBtn = page.locator('aside button', { hasText: 'New session' }).first();
  await navBtn.click({ timeout: 10000 });
  await page.waitForTimeout(2500);
  // Ensure the draft targets the scratch project (default may be another).
  await selectDraftProject(page, scratch);
}

async function selectDraftProject(page, scratch) {
  // The draft header shows a project picker ("Choose project"). Only switch
  // when the current selection isn't already the scratch project.
  const picker = page.locator('button', { hasText: 'Choose project' }).first();
  if (!(await picker.count())) return;
  await picker.click({ timeout: 6000 });
  await page.waitForTimeout(800);
  const name = scratch.split('/').pop();
  const opt = page.getByRole('option', { name }).first();
  if (await opt.count()) {
    await opt.click({ timeout: 6000 });
    await page.waitForTimeout(800);
  } else {
    await page.keyboard.press('Escape');
  }
}

/** The composer editable element (contenteditable in the current UI). */
export function composer(page) {
  return page.locator('[role=textbox], textarea, [contenteditable=true]').first();
}

/** Read the visible transcript text of the open session.
 *  NOTE: portal-rendered overlays (plan review, ask dialogs) are NOT inside
 *  <main> — query [role=dialog] separately when the assertion is about them. */
export async function transcriptText(page) {
  return page.evaluate(() => (document.querySelector('main') ?? document.body).innerText);
}
