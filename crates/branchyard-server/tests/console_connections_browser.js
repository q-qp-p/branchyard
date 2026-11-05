// Drives the Tools page and the composer's Tools control in headless
// Chromium for tests/console_connections_browser.rs, which runs the server
// and a mock connector gateway and reads this script's stdout a line at a
// time: LOADED, TOOLS_PAGE, OAUTH, KEY, POLICY, CONTROL, STARTED, DEFAULTS,
// HELPERS, MINE, INSTALL, PHONE and DONE, each once the page has shown it
// and, where the server decides, its API agrees.
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code; BY_TOKEN a configured token with every scope (used only to
// change state "from another browser"); BY_LINEAR_KEY the key typed for
// Linear; BY_SCREENSHOTS, when set, a directory for screenshots.
'use strict';

const { chromium } = require(process.env.PLAYWRIGHT);

const say = (text) => process.stdout.write(`${text}\n`);
const shots = process.env.BY_SCREENSHOTS;
const base = process.env.BY_URL;
const KEY = process.env.BY_LINEAR_KEY;
const scale = Number(process.env.BY_TEST_TIMEOUT_SCALE) || 1;
const TURN = { timeout: 60000 * scale };
let browser = null;
let shown = null;
const problems = [];

const same = (got, want, what) => {
  if (JSON.stringify(got) !== JSON.stringify(want)) throw new Error(`${what}: ${JSON.stringify(got)}, not ${JSON.stringify(want)}`);
};
// Another client of the same server, as a second browser tab would be.
const other = async (method, path, body) => {
  const res = await fetch(`${base}${path}`, {
    method,
    headers: { authorization: `Bearer ${process.env.BY_TOKEN}`, 'content-type': 'application/json' },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!res.ok) throw new Error(`${method} ${path}: ${res.status} ${await res.text()}`);
  return res.json();
};
// The same, as the paired person (the workspace's owner), from the page's own token.
const mine = (page, method, path, body) => page.evaluate(async ([m, p, b]) => {
  const token = sessionStorage.getItem('branchyard.token');
  const res = await fetch(p, { method: m, headers: { authorization: `Bearer ${token}`, 'content-type': 'application/json' }, body: b ? JSON.stringify(b) : undefined });
  if (!res.ok) throw new Error(`${m} ${p}: ${res.status} ${await res.text()}`);
  return res.json();
}, [method, path, body || null]);
const byKey = (list) => list.slice().sort((a, b) => `${a.kind}:${a.id}`.localeCompare(`${b.kind}:${b.id}`));

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  context.setDefaultTimeout(30000 * scale);
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  // The two stale saves this script makes on purpose answer 409; the browser logs each.
  page.on('console', (m) => { if (m.type() === 'error' && !/status of 409/.test(m.text())) problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));
  const go = (to) => page.evaluate((h) => { location.hash = h; }, to);
  const toast = (text) => page.locator('.toast', { hasText: text }).first().waitFor(TURN);
  const noOverflow = async (what) => {
    if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error(`${what} overflows at ${page.viewportSize().width}px`);
  };
  // Screenshots wait for the menu's entrance to finish (none runs under reduced motion).
  const settled = (el) => el.evaluate((e) => Promise.all(e.getAnimations({ subtree: true }).map((a) => a.finished)));
  const shot = async (target, name) => { if (shots) await target.screenshot({ path: `${shots}/${name}.png` }); };

  await page.goto(`${base}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor();
  say('LOADED');

  // The sidebar has no Connections link; Settings has a Tools page.
  if (await page.locator('#sidebar').getByRole('link', { name: 'Connections', exact: true }).count()) throw new Error('the sidebar still links Connections');
  await page.locator('#sidebar').getByRole('link', { name: 'Settings', exact: true }).click();
  await page.getByRole('heading', { level: 1, name: 'Settings', exact: true }).waitFor();
  const subnav = page.getByRole('navigation', { name: 'Settings sections', exact: true });
  same(await subnav.getByRole('link', { name: 'Account', exact: true }).getAttribute('aria-current'), 'page', 'the Account page');
  await subnav.getByRole('link', { name: 'Tools', exact: true }).click();
  await page.waitForFunction(() => location.hash === '#/settings/tools');
  same(await page.locator('#sidebar a[data-route="settings"]').getAttribute('aria-current'), 'page', 'Settings stays current on its Tools page');
  const table = page.locator('#tools-table');
  const row = (kind, id) => table.locator(`tr#tool-${kind}-${id}`);
  await row('connector', 'atlassian').locator('.conn-state', { hasText: 'Ready' }).waitFor();
  await row('connector', 'github').locator('.conn-state', { hasText: 'Not connected' }).waitFor();
  await row('connector', 'linear').locator('.conn-state', { hasText: 'Not connected' }).waitFor();
  await row('skill', 'docx').locator('.conn-state', { hasText: 'Ready' }).waitFor();
  same(await row('skill', 'docx').locator('td').first().innerText(), 'Skill', 'the kind column');
  await row('helper', 'gemini-cli').locator('.conn-state', { hasText: 'Ready' }).waitFor();
  // An administrator sees the Allowed column; everything is allowed so far.
  await table.getByRole('columnheader', { name: 'Allowed', exact: true }).waitFor();
  await row('connector', 'github').locator('td.tool-allowed', { hasText: /^Allowed$/ }).waitFor();
  if (await row('connector', 'atlassian').getByRole('button').count()) throw new Error('a ready connector offers an action');
  // Filter by kind, then search.
  await page.getByLabel('Kind', { exact: true }).selectOption('skill');
  await row('connector', 'github').waitFor({ state: 'detached' });
  await row('skill', 'docx').waitFor();
  await page.getByLabel('Search tools', { exact: true }).fill('docx');
  await page.locator('#tools-count', { hasText: /^\d+ of \d+ tools/ }).waitFor();
  await row('skill', 'docx').waitFor();
  if (await row('skill', 'pdf').count()) throw new Error('the search does not filter the table');
  await page.getByLabel('Search tools', { exact: true }).fill('');
  await page.getByLabel('Kind', { exact: true }).selectOption('all');
  await row('connector', 'github').waitFor();
  await shot(page, 'tools-page');
  // The old address lands here.
  await go('#/');
  await go('#/connections');
  await page.waitForFunction(() => location.hash === '#/settings/tools');
  await row('connector', 'github').waitFor();
  say('TOOLS_PAGE');

  // GitHub signs in in a new tab; the page sees it finish.
  const [signin] = await Promise.all([
    context.waitForEvent('page'),
    row('connector', 'github').getByRole('button', { name: 'Connect GitHub', exact: true }).click(),
  ]);
  await row('connector', 'github').getByText('Finish signing in to GitHub in the new tab.', { exact: false }).waitFor();
  await signin.waitForLoadState();
  if (!(await signin.content()).includes('Connected github')) throw new Error('the sign-in tab did not reach the gateway');
  await signin.close();
  await toast('GitHub connected.');
  await row('connector', 'github').locator('.conn-state', { hasText: 'Ready' }).waitFor(TURN);
  say('OAUTH');

  // Linear takes a key, typed here and never shown again.
  await row('connector', 'linear').getByRole('button', { name: 'Connect Linear', exact: true }).click();
  const keyField = row('connector', 'linear').getByLabel('Linear API key or token', { exact: true });
  await keyField.waitFor();
  if (await keyField.getAttribute('type') !== 'password') throw new Error('the key field shows the key');
  await row('connector', 'linear').getByRole('button', { name: 'Save key', exact: true }).click();
  await row('connector', 'linear').getByText('Paste the key first.', { exact: true }).waitFor();
  await keyField.fill(KEY);
  await row('connector', 'linear').getByRole('button', { name: 'Save key', exact: true }).click();
  await toast('Linear connected.');
  await row('connector', 'linear').locator('.conn-state', { hasText: 'Ready' }).waitFor(TURN);
  if ((await page.content()).includes(KEY)) throw new Error('the page still holds the key');
  say('KEY');

  // The administrator's editor: only Atlassian, read only, for everyone.
  await page.locator('#policy > summary').click();
  const editor = page.locator('#policy');
  await editor.getByRole('combobox', { name: 'Who this applies to', exact: true }).waitFor();
  await editor.getByRole('radio', { name: 'Only these connectors', exact: true }).check();
  await editor.getByRole('checkbox', { name: 'Atlassian', exact: true }).check();
  await shot(page, 'tools-policy');
  await editor.getByRole('button', { name: 'Save policy', exact: true }).click();
  await toast('Policy saved.');
  await row('connector', 'github').locator('td.tool-allowed', { hasText: /^Not allowed$/ }).waitFor();
  await row('connector', 'atlassian').locator('td.tool-allowed', { hasText: /^Read only$/ }).waitFor();
  await row('connector', 'github').locator('.conn-state', { hasText: 'Not allowed' }).waitFor(TURN);
  await row('connector', 'github').locator('.tool-ask').waitFor();
  same((await other('GET', '/v1/admin/policies')).policies.map((p) => [p.key, p.revision, (p.policy.connectors || []).map((g) => [g.connector, g.mode])]),
    [['tenant:default', 1, [['atlassian', 'read']]]], 'the stored policy');
  // Another admin changes it meanwhile: the stale save is refused, nothing is lost silently.
  await other('PUT', '/v1/admin/policies/tenant:default', { policy: { connectors: [{ connector: 'atlassian', mode: 'write' }, { connector: 'github', mode: 'read' }, { connector: 'linear', mode: 'read' }] }, revision: 1 });
  await editor.getByRole('checkbox', { name: 'GitHub', exact: true }).check();
  await editor.getByRole('button', { name: 'Save policy', exact: true }).click();
  await toast('The policy changed meanwhile.');
  await page.waitForFunction(() => document.getElementById('pol-connectors-item-linear')?.checked === true);
  same(await editor.getByRole('checkbox', { name: 'GitHub', exact: true }).isChecked(), true, 'the reloaded policy');
  // Back to everything allowed, with a save at the current revision.
  await editor.getByRole('radio', { name: 'Allow every connector', exact: true }).check();
  await editor.getByRole('button', { name: 'Save policy', exact: true }).click();
  await toast('Policy saved.');
  await row('connector', 'github').locator('td.tool-allowed', { hasText: /^Allowed$/ }).waitFor();
  await row('connector', 'github').locator('.conn-state', { hasText: 'Ready' }).waitFor(TURN);
  say('POLICY');

  // A knowledge project and workspace, then a Work session with tools.
  const picker = page.locator('#sidebar').getByRole('combobox', { name: 'Source', exact: true });
  await go('#/');
  await picker.locator('option[value="new-project"]').waitFor({ state: 'attached' });
  await picker.selectOption('new-project');
  await page.getByRole('heading', { level: 1, name: 'New project', exact: true }).waitFor();
  const projectForm = page.locator('#main');
  await projectForm.getByLabel('Project name', { exact: true }).fill('Launch brief');
  await projectForm.getByLabel('Source', { exact: true }).selectOption({ label: 'Knowledge work (no repository)' });
  await projectForm.getByRole('button', { name: 'Create project', exact: true }).click();
  await page.waitForFunction(() => /^#\/p\/[^/]+$/.test(location.hash) && location.hash !== '#/p/new', null, TURN);
  await page.locator('#new-task').filter({ hasText: 'New workspace' }).click();
  await page.waitForFunction(() => [...(document.activeElement.labels || [])].some((label) => label.textContent.trim() === 'Workspace title'));
  await page.getByLabel('Workspace title', { exact: true }).fill('Launch');
  await page.getByRole('button', { name: 'Create workspace', exact: true }).click();
  await page.waitForFunction(() => /^#\/w\/[^/]+$/.test(location.hash), null, TURN);
  const ws = new URL(page.url()).hash.slice('#/w/'.length);
  await page.locator('header#strip p.state', { hasText: 'Ready' }).waitFor(TURN);
  const plus = page.locator('nav#sessions').getByRole('button', { name: '+ Session', exact: true });
  if (!(await page.locator('#session-form').count())) await plus.click();
  const form = page.locator('#session-form');
  if (!(await form.getByRole('radio', { name: /^Work/ }).isChecked())) throw new Error('a knowledge workspace does not start in Work');
  // Two harnesses are offered (codex is not installed yet), so the run step stays open on the first.
  same(await form.locator('#sf-harness').inputValue(), 'gemini-cli', 'the harness the form starts with');
  // No separate Tools step: one button beside the message box.
  if (await form.locator('fieldset > legend', { hasText: /^Tools$/ }).count()) throw new Error('the Tools fieldset is still in the form');
  const button = form.getByRole('button', { name: /^Tools:/ });
  await button.getByText('Tools', { exact: true }).waitFor();
  same(await button.getAttribute('aria-label'), 'Tools: none chosen', 'the empty button');
  same(await button.getAttribute('aria-expanded'), 'false', 'the closed menu');
  await form.locator('#sf-tools-summary', { hasText: 'Just its files' }).waitFor();
  const menu = form.locator('#sf-tools-menu');
  if (await menu.isVisible()) throw new Error('the menu is open before it is asked for');
  await button.click();
  same(await button.getAttribute('aria-expanded'), 'true', 'the open menu');
  same(await menu.getAttribute('role'), 'group', 'the wide menu is a group');
  same(await menu.getAttribute('aria-label'), 'Tools', 'the wide menu is named');
  if (!(await form.locator('#sf-tools-search').evaluate((el) => el === document.activeElement))) throw new Error('the search field does not take focus');
  for (const group of ['Connectors', 'Skills', 'Helpers']) await menu.getByRole('heading', { level: 4, name: new RegExp(`^${group} \\(`) }).waitFor();
  // Atlassian can make changes; GitHub reads only; one skill; one helper.
  await menu.getByRole('checkbox', { name: 'Atlassian', exact: true }).check();
  await menu.getByRole('radio', { name: 'Can make changes', exact: true }).check();
  await menu.getByText('It may change things in Atlassian.', { exact: false }).waitFor();
  await menu.getByRole('checkbox', { name: 'GitHub', exact: true }).check();
  await menu.getByLabel('Search tools', { exact: true }).fill('docx');
  await menu.locator('#sf-tools-count', { hasText: /shown, 2 chosen\./ }).waitFor();
  if (await menu.getByRole('checkbox', { name: 'GitHub', exact: true }).count()) throw new Error('the search does not filter');
  await menu.getByRole('checkbox', { name: 'docx', exact: true }).check();
  await menu.getByLabel('Search tools', { exact: true }).fill('gemini');
  await menu.getByRole('checkbox', { name: 'gemini-cli', exact: true }).check();
  await menu.getByLabel('Search tools', { exact: true }).fill('');
  await menu.getByRole('checkbox', { name: 'GitHub', exact: true }).waitFor();
  same(await button.getAttribute('aria-label'), 'Tools: Atlassian, GitHub, docx, gemini-cli', 'the button names the tools');
  same(await form.locator('.tool-stack .conn-mark').count(), 4, 'the stacked marks');
  await settled(menu);
  await shot(form, 'composer-menu');
  // More than four: the stack shows four and "+N".
  await menu.getByRole('checkbox', { name: 'pdf', exact: true }).check();
  await form.locator('.tool-more', { hasText: '+1' }).waitFor();
  same(await form.locator('.tool-stack .conn-mark').count(), 4, 'at most four marks');
  await menu.getByRole('checkbox', { name: 'pdf', exact: true }).uncheck();
  // Escape closes it and gives the focus back.
  await page.keyboard.press('Escape');
  await menu.waitFor({ state: 'hidden' });
  if (!(await button.evaluate((el) => el === document.activeElement))) throw new Error('Escape does not return focus to the button');
  await form.locator('#sf-tools-summary', { hasText: 'Atlassian (can make changes), GitHub (read only), docx and 1 more.' }).waitFor();
  await shot(form, 'composer-chosen');
  say('CONTROL');

  await form.getByLabel('First prompt', { exact: true }).fill('Summarise the open launch epics into a one-page brief.\nUse the tracker.');
  const [startRequest, started] = await Promise.all([
    page.waitForRequest((r) => r.method() === 'POST' && new URL(r.url()).pathname === `/v1/workspaces/${ws}/sessions`),
    page.waitForResponse((r) => r.request().method() === 'POST' && new URL(r.url()).pathname === `/v1/workspaces/${ws}/sessions`),
    form.getByRole('button', { name: 'Start session', exact: true }).click(),
  ]);
  const body = startRequest.postDataJSON();
  same([body.name, !!body.result_contract, 'connectors' in body, 'skills' in body, 'helpers' in body],
    ['Summarise the open launch epics into a one-page brief.', true, false, false, false], 'the start carries the single field');
  same(byKey(body.tools), byKey([
    { kind: 'connector', id: 'atlassian', mode: 'write' }, { kind: 'connector', id: 'github', mode: 'read' },
    { kind: 'skill', id: 'docx' }, { kind: 'helper', id: 'gemini-cli' }]), 'the start\'s tools');
  if (started.status() !== 200) throw new Error(`the start answered ${started.status()} ${await started.text()}`);
  await toast('Session started.');
  const facts = page.locator('#session-facts');
  await facts.getByText('Atlassian (can make changes)', { exact: true }).waitFor(TURN);
  await facts.getByText('GitHub (read only)', { exact: true }).waitFor();
  await facts.getByText('1 skill', { exact: true }).waitFor();
  await facts.getByText('helpers: gemini-cli', { exact: true }).waitFor();
  // No defaults are saved yet, so the next draft falls back to this browser's choice.
  await plus.click();
  await form.getByRole('button', { name: 'Tools: Atlassian, GitHub, docx, gemini-cli', exact: true }).waitFor();
  say('STARTED');

  // Save the choice as the workspace's default; it comes from the server, not the browser.
  const next = form.getByRole('button', { name: /^Tools:/ });
  await next.click();
  await menu.getByRole('button', { name: 'Save as this workspace\'s default', exact: true }).click();
  await menu.getByText('Saved. New sessions here start with these tools.', { exact: true }).waitFor();
  const saved = (await mine(page, 'GET', `/v1/workspaces/${ws}/tools`)).defaults;
  same([saved.revision, byKey(saved.tools)], [1, byKey([
    { kind: 'connector', id: 'atlassian', mode: 'write' }, { kind: 'connector', id: 'github', mode: 'read' },
    { kind: 'skill', id: 'docx' }, { kind: 'helper', id: 'gemini-cli' }])], 'the saved defaults');
  if (!(await menu.getByRole('button', { name: 'Save as this workspace\'s default', exact: true }).isDisabled())) throw new Error('an unchanged choice can be saved again');
  await page.keyboard.press('Escape');
  await page.evaluate(() => localStorage.clear());
  await page.reload();
  await page.getByRole('heading', { level: 1 }).first().waitFor();
  if (!(await form.count())) await plus.click();
  await form.getByRole('button', { name: 'Tools: Atlassian, GitHub, docx, gemini-cli', exact: true }).waitFor();
  await form.locator('#sf-tools-summary', { hasText: 'From this workspace\'s defaults.' }).waitFor();
  // Someone else changes the defaults: this tab's save is refused and reloaded.
  await mine(page, 'PUT', `/v1/workspaces/${ws}/tools`, { tools: [{ kind: 'skill', id: 'pdf' }], revision: 1 });
  await form.getByRole('button', { name: /^Tools:/ }).click();
  await menu.getByRole('checkbox', { name: 'GitHub', exact: true }).uncheck();
  await menu.getByRole('button', { name: 'Save as this workspace\'s default', exact: true }).click();
  await menu.getByText('The defaults changed meanwhile.', { exact: false }).waitFor();
  same((await mine(page, 'GET', `/v1/workspaces/${ws}/tools`)).defaults.revision, 2, 'the other client\'s defaults stand');
  await page.keyboard.press('Escape');
  say('DEFAULTS');

  // The helper limit: a number that waits for a helper, rides in `tools` as
  // the `*` entry, and stops at the administrator's `max_helpers`.
  await page.reload();
  await page.getByRole('heading', { level: 1 }).first().waitFor();
  if (!(await form.count())) await plus.click();
  await form.getByLabel('First prompt', { exact: true }).fill('Split the audit across two helpers.');
  await form.getByRole('button', { name: /^Tools:/ }).click();
  const limit = menu.getByLabel('Most helpers at once', { exact: true });
  await limit.waitFor();
  same(await limit.isDisabled(), true, 'the limit waits for a helper');
  await menu.getByText('Choose a helper first.', { exact: true }).waitFor();
  await menu.getByRole('checkbox', { name: 'gemini-cli', exact: true }).check();
  same(await limit.isDisabled(), false, 'a helper enables the limit');
  same([await limit.getAttribute('placeholder'), await limit.getAttribute('max')], ['4', '16'], 'the default and the top');
  await limit.fill('2');
  await limit.press('Tab');
  await menu.getByText('From 1 to 16. Left empty, a session may have 4.', { exact: true }).waitFor();
  await page.keyboard.press('Escape');
  const [limited] = await Promise.all([
    page.waitForRequest((r) => r.method() === 'POST' && new URL(r.url()).pathname === `/v1/workspaces/${ws}/sessions`),
    form.getByRole('button', { name: 'Start session', exact: true }).click(),
  ]);
  const limitedTools = limited.postDataJSON().tools;
  same(limitedTools.filter((t) => t.kind === 'helper'), [{ kind: 'helper', id: 'gemini-cli' }, { kind: 'helper', id: '*', max_children: 2 }], 'the limit rides in tools');
  await toast('Session started.');
  await page.locator('#session-facts').getByText('helpers: gemini-cli, at most 2 at once', { exact: true }).waitFor(TURN);
  // The administrator lowers the top to 3: the control stops there.
  const policies = (await other('GET', '/v1/admin/policies')).policies.find((p) => p.key === 'tenant:default');
  await other('PUT', '/v1/admin/policies/tenant:default', { policy: { ...policies.policy, max_helpers: 3 }, revision: policies.revision });
  await page.reload();
  await page.getByRole('heading', { level: 1 }).first().waitFor();
  if (!(await form.count())) await plus.click();
  await form.getByRole('button', { name: /^Tools:/ }).click();
  await menu.getByRole('checkbox', { name: 'gemini-cli', exact: true }).check();
  const capped = menu.getByLabel('Most helpers at once', { exact: true });
  same([await capped.getAttribute('max'), await capped.getAttribute('placeholder')], ['3', '3'], 'the administrator\'s top');
  await menu.getByText('Your administrator sets the top.', { exact: false }).waitFor();
  await capped.fill('9');
  await capped.press('Tab');
  same(await capped.inputValue(), '3', 'a number above the top is held to it');
  await page.keyboard.press('Escape');
  // Back to no cap.
  const capRecord = (await other('GET', '/v1/admin/policies')).policies.find((p) => p.key === 'tenant:default');
  const { max_helpers: _gone, ...uncapped } = capRecord.policy;
  await other('PUT', '/v1/admin/policies/tenant:default', { policy: uncapped, revision: capRecord.revision });
  say('HELPERS');

  // A person's own policy: the Mine column switches a skill off for them.
  await go('#/settings/tools');
  await page.getByRole('heading', { level: 1, name: 'Settings', exact: true }).waitFor();
  await table.getByRole('columnheader', { name: 'Mine', exact: true }).waitFor();
  const docxMine = row('skill', 'docx').getByRole('checkbox', { name: 'Use docx', exact: true });
  same(await docxMine.isChecked(), true, 'docx is on for me');
  await docxMine.uncheck();
  await toast('docx is off for you.');
  await row('skill', 'docx').locator('.conn-state', { hasText: 'Off for you' }).waitFor(TURN);
  await row('skill', 'docx').getByText('You turned this off for yourself.', { exact: false }).waitFor();
  await row('skill', 'docx').getByRole('button', { name: 'Turn on docx', exact: true }).waitFor();
  same((await mine(page, 'GET', '/v1/me/policy')).policy, { off: [{ kind: 'skill', id: 'docx' }] }, 'my stored policy');
  same((await other('GET', '/v1/admin/policies')).policies.some((p) => p.key.startsWith('self:')), false, 'the administrator\'s list does not hold it');
  // The administrator's policy stays the ceiling: what it withholds has no switch.
  const current = (await other('GET', '/v1/admin/policies')).policies.find((p) => p.key === 'tenant:default');
  await other('PUT', '/v1/admin/policies/tenant:default', { policy: { ...current.policy, skills: ['pdf'] }, revision: current.revision });
  await page.reload();
  await row('skill', 'pptx').locator('.conn-state', { hasText: 'Not allowed' }).waitFor(TURN);
  await row('skill', 'pptx').locator('td.tool-mine', { hasText: 'Not allowed' }).waitFor();
  same(await row('skill', 'pptx').getByRole('checkbox').count(), 0, 'no switch for what the administrator withholds');
  await row('skill', 'pdf').getByRole('checkbox', { name: 'Use pdf', exact: true }).waitFor();
  const lifted = (await other('GET', '/v1/admin/policies')).policies.find((p) => p.key === 'tenant:default');
  const { skills: _skills, ...open } = lifted.policy;
  await other('PUT', '/v1/admin/policies/tenant:default', { policy: open, revision: lifted.revision });
  await page.reload();
  await row('skill', 'docx').getByRole('button', { name: 'Turn on docx', exact: true }).click();
  await toast('docx is on for you.');
  await row('skill', 'docx').locator('.conn-state', { hasText: 'Ready' }).waitFor(TURN);
  same((await mine(page, 'GET', '/v1/me/policy')).policy, {}, 'turned back on');
  say('MINE');

  // Installing a helper's harness: on the server's host, from its row.
  await row('helper', 'codex').locator('.conn-state', { hasText: 'Not available' }).waitFor();
  await row('helper', 'codex').getByText('On the server only', { exact: true }).waitFor();
  await row('helper', 'codex').getByRole('button', { name: 'Install codex', exact: true }).click();
  await toast('codex installed on the server.');
  await row('helper', 'codex').locator('.conn-state', { hasText: 'Ready' }).waitFor(TURN);
  same((await mine(page, 'GET', '/v1/tools/helpers/codex/install')).state, 'succeeded', 'the install ended');
  if (await row('helper', 'codex').getByRole('button').count()) throw new Error('an installed helper still offers Install');
  say('INSTALL');

  // A phone: the menu is a modal sheet that holds focus; Escape closes it.
  await go(`#/w/${ws}`);
  if (!(await form.count())) await plus.click();
  await page.setViewportSize({ width: 390, height: 844 });
  const phoneButton = form.getByRole('button', { name: /^Tools:/ });
  await phoneButton.click();
  same(await menu.getAttribute('role'), 'dialog', 'the narrow menu is a dialog');
  same(await menu.getAttribute('aria-modal'), 'true', 'the narrow menu is modal');
  await menu.getByRole('heading', { level: 3, name: 'Tools', exact: true }).waitFor();
  await noOverflow('the Tools sheet');
  const box = await menu.boundingBox();
  // (a few pixels of slack: the menu may still be settling into place)
  if (Math.abs(box.y + box.height - 844) > 8 || box.width < 370) throw new Error(`the sheet is not at the bottom: ${JSON.stringify(box)}`);
  for (let i = 0; i < 40; i += 1) {
    await page.keyboard.press('Tab');
    if (!(await menu.evaluate((el) => el.contains(document.activeElement)))) throw new Error('Tab left the sheet');
  }
  for (let i = 0; i < 5; i += 1) {
    await page.keyboard.press('Shift+Tab');
    if (!(await menu.evaluate((el) => el.contains(document.activeElement)))) throw new Error('Shift+Tab left the sheet');
  }
  await settled(menu);
  await shot(page, 'composer-sheet-phone');
  await page.keyboard.press('Escape');
  await menu.waitFor({ state: 'hidden' });
  if (!(await phoneButton.evaluate((el) => el === document.activeElement))) throw new Error('Escape does not return focus to the button on a phone');
  await noOverflow('the session form');
  await go('#/settings/tools');
  await page.getByRole('heading', { level: 1, name: 'Settings', exact: true }).waitFor();
  await row('connector', 'github').locator('.conn-state', { hasText: 'Ready' }).waitFor();
  await noOverflow('the Tools page');
  await shot(page, 'tools-page-phone');
  say('PHONE');

  // Screenshots of both pages at both widths, light and dark, for review.
  if (shots) {
    for (const scheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const width of [1440, 390]) {
        await page.setViewportSize({ width, height: width === 390 ? 844 : 900 });
        await go('#/settings/tools');
        await row('connector', 'github').locator('.conn-state', { hasText: 'Ready' }).waitFor();
        await page.screenshot({ path: `${shots}/tools-page-${width}-${scheme}.png` });
        await go(`#/w/${ws}`);
        if (!(await page.locator('#session-form').count())) await plus.click();
        const open = form.getByRole('button', { name: /^Tools:/ });
        await open.waitFor();
        await open.scrollIntoViewIfNeeded();
        await open.click();
        await menu.waitFor();
        await settled(menu);
        await page.screenshot({ path: `${shots}/composer-menu-${width}-${scheme}.png` });
        await page.keyboard.press('Escape');
        await menu.waitFor({ state: 'hidden' });
      }
    }
  }

  if (problems.length) throw new Error(`the page reported errors: ${problems.join('; ')}`);
  say('DONE');
  await browser.close();
})().catch(async (e) => {
  console.error(e);
  if (shown) console.error(`the page showed:\n${await shown().catch(() => '(nothing)')}`);
  if (browser) await browser.close();
  process.exit(1);
});
