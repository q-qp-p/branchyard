// Drives Connections and a session's tools in headless Chromium for
// tests/console_connections_browser.rs, which runs the server and a mock
// connector gateway and reads this script's stdout a line at a time:
// LOADED, CONNECTIONS, OAUTH, KEY, TOOLS, STARTED, PHONE and DONE, each once
// the page has shown it and, where the server decides, its API agrees.
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code; BY_LINEAR_KEY the key typed for Linear; BY_SCREENSHOTS,
// when set, a directory for screenshots.
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

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  context.setDefaultTimeout(30000 * scale);
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));
  const go = (to) => page.evaluate((h) => { location.hash = h; }, to);
  const toast = (text) => page.locator('.toast', { hasText: text }).first().waitFor(TURN);
  const noOverflow = async (what) => {
    if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error(`${what} overflows at ${page.viewportSize().width}px`);
  };

  await page.goto(`${base}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor();
  say('LOADED');

  // The Connections page, from the sidebar.
  await page.locator('#sidebar').getByRole('link', { name: 'Connections', exact: true }).click();
  await page.getByRole('heading', { level: 1, name: 'Connections', exact: true }).waitFor();
  const card = (name) => page.locator('li.conn-card').filter({ has: page.getByRole('heading', { level: 3, name, exact: true }) });
  await card('Atlassian').locator('.conn-state', { hasText: 'Connected' }).waitFor();
  await card('Github').locator('.conn-state', { hasText: 'Not connected' }).waitFor();
  await card('Linear').locator('.conn-state', { hasText: 'Not connected' }).waitFor();
  await page.locator('.conn-status', { hasText: '1 connection ready of 3 this server offers.' }).waitFor();
  if (await card('Atlassian').getByRole('button').count()) throw new Error('a connected connection offers an action');
  if (shots) await page.screenshot({ path: `${shots}/connections.png` });
  say('CONNECTIONS');

  // GitHub (no catalogue entry, so named from its ID) signs in in a new tab; the page sees it finish.
  const [signin] = await Promise.all([
    context.waitForEvent('page'),
    card('Github').getByRole('button', { name: 'Connect', exact: true }).click(),
  ]);
  await card('Github').getByText('Finish signing in to Github in the new tab.', { exact: false }).waitFor();
  await signin.waitForLoadState();
  if (!(await signin.content()).includes('Connected github')) throw new Error('the sign-in tab did not reach the gateway');
  await signin.close();
  await toast('Github connected.');
  await card('Github').locator('.conn-state', { hasText: 'Connected' }).waitFor(TURN);
  say('OAUTH');

  // Linear takes a key, typed here and never shown again.
  await card('Linear').getByRole('button', { name: 'Connect', exact: true }).click();
  const keyField = card('Linear').getByLabel('Linear API key or token', { exact: true });
  await keyField.waitFor();
  if (await keyField.getAttribute('type') !== 'password') throw new Error('the key field shows the key');
  await card('Linear').getByRole('button', { name: 'Save key', exact: true }).click();
  await card('Linear').getByText('Paste the key first.', { exact: true }).waitFor();
  await keyField.fill(KEY);
  await card('Linear').getByRole('button', { name: 'Save key', exact: true }).click();
  await toast('Linear connected.');
  await card('Linear').locator('.conn-state', { hasText: 'Connected' }).waitFor(TURN);
  if ((await page.content()).includes(KEY)) throw new Error('the page still holds the key');
  say('KEY');

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
  // One environment and one harness: nothing to choose, so one line.
  await form.locator('#sf-run-line', { hasText: 'Runs in Local workers with gemini-cli.' }).waitFor();
  if (await form.locator('#sf-environments').isVisible()) throw new Error('Work shows a choice that is not one');
  const tools = form.locator('#sf-tools');
  await tools.getByText('Only its files and every how-to.', { exact: false }).waitFor();
  const toggle = tools.getByRole('button', { name: 'Choose tools', exact: true });
  same(await toggle.getAttribute('aria-expanded'), 'false', 'the closed panel');
  await toggle.click();
  const panel = form.locator('#sf-tools-panel');
  await panel.getByRole('checkbox', { name: 'Atlassian', exact: true }).check();
  await panel.getByRole('radio', { name: 'Can make changes', exact: true }).check();
  await panel.getByText('It may change things in Atlassian.', { exact: false }).waitFor();
  await panel.getByRole('checkbox', { name: 'Github', exact: true }).check();
  await panel.getByRole('radio', { name: 'Only the ones I choose', exact: true }).check();
  await panel.getByRole('checkbox', { name: 'docx', exact: true }).check();
  await panel.getByRole('checkbox', { name: 'Let it hand parts of the work to helpers', exact: true }).check();
  await panel.getByRole('checkbox', { name: 'gemini-cli', exact: true }).check();
  await panel.getByLabel('At most at once', { exact: true }).fill('2');
  await panel.getByLabel('At most at once', { exact: true }).press('Tab');
  await tools.locator('#sf-chip-atlassian', { hasText: 'Atlassian · can make changes' }).waitFor();
  await tools.locator('#sf-chip-github', { hasText: 'Github · read only' }).waitFor();
  await tools.getByText('1 how-to', { exact: true }).waitFor();
  if (shots) await form.screenshot({ path: `${shots}/session-tools.png` });
  await tools.getByRole('button', { name: 'Done', exact: true }).click();
  if (shots) await form.screenshot({ path: `${shots}/session-tools-chosen.png` });
  same(await tools.getByRole('button', { name: 'Change tools', exact: true }).getAttribute('aria-expanded'), 'false', 'the panel once done');
  say('TOOLS');

  await form.getByLabel('First prompt', { exact: true }).fill('Summarise the open launch epics into a one-page brief.\nUse the tracker.');
  const [startRequest, started] = await Promise.all([
    page.waitForRequest((r) => r.method() === 'POST' && new URL(r.url()).pathname === `/v1/workspaces/${ws}/sessions`),
    page.waitForResponse((r) => r.request().method() === 'POST' && new URL(r.url()).pathname === `/v1/workspaces/${ws}/sessions`),
    form.getByRole('button', { name: 'Start session', exact: true }).click(),
  ]);
  const body = startRequest.postDataJSON();
  same([body.name, body.connectors, body.skills, body.helpers, !!body.result_contract],
    ['Summarise the open launch epics into a one-page brief.', ['atlassian:write', 'github'], ['docx'], { harnesses: ['gemini-cli'], max_children: 2 }, true],
    'the start');
  if (started.status() !== 200) throw new Error(`the start answered ${started.status()} ${await started.text()}`);
  await toast('Session started.');
  const facts = page.locator('#session-facts');
  await facts.getByText('Atlassian (can make changes)', { exact: true }).waitFor(TURN);
  await facts.getByText('Github (read only)', { exact: true }).waitFor();
  await facts.getByText('1 how-to', { exact: true }).waitFor();
  await facts.getByText('helpers: gemini-cli', { exact: true }).waitFor();
  // The next draft remembers the connections and the how-tos.
  await plus.click();
  await form.locator('#sf-chip-atlassian', { hasText: 'Atlassian · can make changes' }).waitFor();
  await form.locator('#sf-chip-github').waitFor();
  say('STARTED');

  await page.setViewportSize({ width: 390, height: 844 });
  await form.locator('#sf-tools').getByRole('button', { name: 'Change tools', exact: true }).click();
  await form.locator('#sf-tools-panel').waitFor();
  await noOverflow('the Tools step');
  await go('#/connections');
  await page.getByRole('heading', { level: 1, name: 'Connections', exact: true }).waitFor();
  await card('Github').locator('.conn-state', { hasText: 'Connected' }).waitFor();
  await noOverflow('the Connections page');
  if (shots) await page.screenshot({ path: `${shots}/connections-phone.png` });
  say('PHONE');

  if (problems.length) throw new Error(`the page reported errors: ${problems.join('; ')}`);
  say('DONE');
  await browser.close();
})().catch(async (e) => {
  console.error(e);
  if (shown) console.error(`the page showed:\n${await shown().catch(() => '(nothing)')}`);
  if (browser) await browser.close();
  process.exit(1);
});
