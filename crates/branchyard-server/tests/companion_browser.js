// Drives the companion page in headless Chromium for
// tests/companion_browser.rs, which runs the server and talks to this
// script over stdin and stdout, a line at a time:
//
//   script: LOADED            the page paired and shows no branches and no problems
//   test:   (submits a task named "first" through the API)
//   script: SEEN first        the branch appeared over the event stream
//   script: FILTERED          a search is focused with selected text
//   test:   (starts another branch), then "refresh"
//   script: FILTER PRESERVED  the live update kept the query and selection
//   script: SENT              a follow-up was sent from the branch page
//   test:   (starts a branch whose reply is markdown), then "report"
//   script: RENDERED          the reply's markdown and JSON are elements
//   test:   (runs two turns of a harness that reports a cost), then "costly"
//   script: COSTED            each turn's receipt shows its own cost
//   test:   (starts a branch whose policy asks before a write), then "asker"
//   script: DECIDED           the deck showed the ask and Enter allowed it
//   test:   (starts "rival", which writes first's a.txt too), then "ground"
//   script: GROUND            first's Ground names rival, its merge card too
//                             (and, merged, first's Ground holds nothing)
//   test:   (queues work requiring a label no worker carries), then "problem <id>"
//   script: PROBLEM           after a reload, home lists it above the tiles with its
//                             error and what to do
//   test:   (a parent runs a child that asks it a question and waits), then "question"
//   script: ASKED             the tab title counts it live; after a reload the
//                             deck's first card is the question, naming child and text
//   test:   (the child is still parked), then "answer"
//   script: ANSWERED          Enter in the card's answer box sent it, the card left the
//                             deck with nothing held, and the title stopped counting it
//   script: DONE              and the page shows its second turn
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code.
'use strict';

const { chromium } = require(process.env.PLAYWRIGHT);
const readline = require('readline');

const lines = readline.createInterface({ input: process.stdin });
const waiting = [];
// A line that arrives before the script asks for it waits its turn.
const arrived = [];
let closed = false;
lines.on('line', (line) => { const w = waiting.shift(); if (w) w.resolve(line); else arrived.push(line); });
lines.on('close', () => { closed = true; for (const w of waiting.splice(0)) w.reject(new Error('the test controller closed stdin')); });
const nextLine = () => (arrived.length ? Promise.resolve(arrived.shift())
  : closed ? Promise.reject(new Error('the test controller closed stdin'))
    : new Promise((resolve, reject) => waiting.push({ resolve, reject })));
let shown = null;
let browser = null;
const say = (text) => process.stdout.write(`${text}\n`);
const shots = process.env.BY_SCREENSHOTS;

(async () => {
  browser = await chromium.launch();
  const context = await browser.newContext();
  const page = await context.newPage();
  shown = () => page.locator('main').innerText();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));
  const go = (hash) => page.evaluate((h) => { location.hash = h; }, hash);
  const noOverflow = async (what) => {
    if (await page.evaluate(() => document.documentElement.scrollWidth > innerWidth)) throw new Error(`${what} overflows at ${page.viewportSize().width}px`);
  };

  await page.goto(`${process.env.BY_URL}/app/`);
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
  await page.getByLabel('Server token', { exact: true }).fill('invalid-fixture-only');
  await page.getByRole('button', { name: 'Connect to server', exact: true }).click();
  await page.getByRole('alert').filter({ hasText: 'That token is not accepted.' }).waitFor();
  if (await page.getByRole('button', { name: 'Connect to server', exact: true }).isDisabled()) throw new Error('failed sign-in left the connect button disabled');

  await page.goto(`${process.env.BY_URL}/app/#pair=${process.env.BY_CODE}`);
  await page.getByRole('heading', { level: 1, name: 'app', exact: true }).waitFor({ timeout: 30000 });
  // The code is gone from the address bar once redeemed.
  if (page.url().includes('pair=')) throw new Error(`the code stayed in the URL: ${page.url()}`);
  await page.getByText('No branches yet.').waitFor();
  await page.getByRole('link', { name: 'New task', exact: true }).waitFor();

  // A failed list request has its own retry, and never looks like an empty yard.
  let failList = true;
  await page.route('**/v1/repos/app/branches', async (route) => {
    if (failList) {
      failList = false;
      await route.fulfill({ status: 503, contentType: 'application/json', body: JSON.stringify({ error: { code: 'unavailable', message: 'Fixture server temporarily unavailable.' } }) });
    } else await route.continue();
  });
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Could not load branches' }).waitFor();
  if (await page.getByText('No branches yet.').count()) throw new Error('a failed request looked empty');
  if (shots) await page.screenshot({ path: `${shots}/error-desktop.png` });
  await page.getByRole('button', { name: 'Retry loading branches' }).click();
  await page.getByText('No branches yet.').waitFor();
  await page.unroute('**/v1/repos/app/branches');

  // Keep the response pending to observe an actual loading state, without a sleep.
  let releaseList;
  const heldList = new Promise((resolve) => { releaseList = resolve; });
  await page.route('**/v1/repos/app/branches', async (route) => { await heldList; await route.continue(); });
  const problemsRead = page.waitForResponse((r) => r.url().endsWith('/v1/problems'));
  await page.reload({ waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Loading branches…' }).waitFor();
  if (shots) await page.screenshot({ path: `${shots}/loading-desktop.png` });
  releaseList();
  await page.getByText('No branches yet.').waitFor();
  await page.unroute('**/v1/repos/app/branches');
  if (shots) await page.screenshot({ path: `${shots}/empty-desktop.png` });
  await page.locator('#live[data-state="live"]').waitFor({ timeout: 30000 });
  if ((await (await problemsRead).json()).problems.length) throw new Error('the fixture server starts with problems');
  if (await page.locator('[aria-label="Problems"]').count()) throw new Error('a healthy server shows a problems strip');
  say('LOADED');

  await nextLine();
  const item = page.locator('#lane-ready a', { hasText: 'first' });
  await item.waitFor({ timeout: 60000 });
  if (await page.locator('.toast').count()) throw new Error(`a stream event raised a toast: ${await page.locator('.toast').allInnerTexts()}`);
  if (await page.title() !== 'Branchyard') throw new Error(`the title counts work that does not need you: ${await page.title()}`);
  say('SEEN first');

  const search = page.getByRole('searchbox', { name: 'Filter branches' });
  await search.fill('FIRST');
  await page.locator('#branch-count', { hasText: 'Showing 1 of 1 branches' }).waitFor();
  await search.fill('unmatched-query');
  await page.getByRole('heading', { name: 'No matching branches' }).waitFor();
  await page.getByRole('button', { name: 'Clear search' }).click();
  await item.waitFor();
  if (!(await search.evaluate((input) => input === document.activeElement))) throw new Error('clearing search lost keyboard focus');
  await search.fill('first');
  await search.evaluate((input) => input.setSelectionRange(1, 3));
  say('FILTERED');
  await nextLine();
  await page.locator('#branch-count', { hasText: 'Showing 1 of 2 branches' }).waitFor();
  const selection = await search.evaluate((input) => [input === document.activeElement, input.selectionStart, input.selectionEnd, input.value]);
  if (JSON.stringify(selection) !== JSON.stringify([true, 1, 3, 'first'])) throw new Error(`live refresh changed the search selection: ${JSON.stringify(selection)}`);
  await search.fill('');
  await page.locator('#branch-count', { hasText: '2 branches' }).waitFor();
  say('FILTER PRESERVED');

  await page.locator('#main h1').focus();
  await page.keyboard.press('/');
  if (!(await search.evaluate((input) => input === document.activeElement))) throw new Error('/ did not focus the filter');
  await page.locator('#main h1').focus();
  const rows = page.locator('#branch-list a');
  const focusedRow = () => page.evaluate(() => document.activeElement.dataset.branch || null);
  await page.keyboard.press('j');
  await page.keyboard.press('j');
  const second = await rows.nth(1).getAttribute('data-branch');
  if (await focusedRow() !== second) throw new Error(`j j focused ${await focusedRow()}, not ${second}`);
  await page.keyboard.press('k');
  const top = await rows.nth(0).getAttribute('data-branch');
  if (await focusedRow() !== top) throw new Error(`k focused ${await focusedRow()}, not ${top}`);
  await page.keyboard.press('Enter');
  await page.locator('#strip h1', { hasText: top }).waitFor();

  await go('#/');
  for (const width of [320, 390]) {
    await page.setViewportSize({ width, height: 844 });
    await item.waitFor();
    await noOverflow('the branch list');
    for (const link of await page.locator('#sidebar nav a').all()) {
      const box = await link.boundingBox();
      if (!box || box.x < 0 || box.x + box.width > width + 1) throw new Error(`navigation is clipped at ${width}px`);
    }
    await item.click();
    await page.getByRole('link', { name: '‹ Branches' }).waitFor();
    await page.locator('#strip').getByRole('button', { name: 'Merge', exact: true }).waitFor();
    await noOverflow('a branch');
    await page.getByRole('link', { name: '‹ Branches' }).click();
    await item.waitFor();
  }

  await page.setViewportSize({ width: 1280, height: 900 });
  if (shots) await page.screenshot({ path: `${shots}/branches-desktop.png` });
  await item.click();
  await page.getByRole('heading', { name: 'first', exact: true }).waitFor();
  const [side, focus, inspector] = await Promise.all(['#sidebar', '.focus', '#inspector'].map((s) => page.locator(s).boundingBox()));
  if (!(side.x + side.width <= focus.x + 1 && focus.x + focus.width <= inspector.x + 1 && Math.abs(focus.y - inspector.y) < 2 && Math.abs(side.y - focus.y) < 2)) {
    throw new Error(`the three panes are not side by side at 1280px: ${JSON.stringify([side, focus, inspector])}`);
  }
  await page.locator('#strip').getByRole('button', { name: 'Merge', exact: true }).waitFor();
  if (await page.getByText('Not now:').count()) throw new Error('an action that does not apply is still explained');
  await page.getByRole('button', { name: 'Resume', exact: true }).waitFor({ state: 'detached' });
  await page.locator('#strip summary[aria-label="More actions"]').click();
  await page.getByRole('button', { name: 'Fork…', exact: true }).waitFor();
  if (await page.getByRole('button', { name: 'Cancel turn' }).count()) throw new Error('Cancel turn is offered for a ready branch');
  await page.keyboard.press('Escape');
  await page.getByRole('button', { name: 'Fork…', exact: true }).waitFor({ state: 'hidden' });

  await page.locator('#convo .you', { hasText: 'WRITE a.txt=1' }).waitFor();
  await page.locator('#convo .agent', { hasText: 'wrote a.txt' }).waitFor();
  if ((await page.locator('#convo').innerText()).includes('session')) throw new Error('session bookkeeping is in the conversation');
  await page.locator('#tab-changes[aria-selected="true"]').waitFor();
  await page.locator('pre.diff span.add', { hasText: '+1' }).waitFor({ timeout: 30000 });

  await page.getByLabel('Send a follow-up prompt').fill('PERMISSION_BY_ID WRITE b.txt=2');
  await page.getByLabel('Allow every tool').check();
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  await page.locator('.toast', { hasText: 'Sent. A new turn is starting.' }).first().waitFor({ timeout: 30000 });
  say('SENT');
  await page.waitForFunction(() => {
    const dt = Array.from(document.querySelectorAll('dl.facts dt')).find((d) => d.textContent === 'Turns');
    return dt && dt.nextElementSibling.textContent === '2';
  }, null, { timeout: 60000 });
  await page.locator('#convo .you', { hasText: 'PERMISSION_BY_ID WRITE b.txt=2' }).waitFor({ timeout: 60000 });

  // The second turn's tool call is a note in the margin beside the text it
  // preceded, named after the tool; it opens the Activity tab.
  const note = page.locator('#convo .turn .notes .note', { hasText: 'bash' }).last();
  await note.waitFor({ timeout: 60000 });
  const notes = page.locator('#convo .turn').last().locator('.notes .note');
  if (await notes.count() !== 1) throw new Error(`the second turn shows ${await notes.count()} notes, not 1`);
  if (await page.locator('#convo details.tools').count()) throw new Error('tool calls are still folded');
  await note.click();
  await page.locator('#tab-activity[aria-selected="true"]').waitFor();
  await page.getByRole('tab', { name: 'Changes' }).click();

  await page.locator('#files button', { hasText: 'b.txt' }).click({ timeout: 30000 });
  await page.locator('pre.diff span.add', { hasText: '+2' }).waitFor({ timeout: 30000 });
  await page.getByRole('tab', { name: 'Activity' }).click();
  await page.locator('#events', { hasText: 'prompt: PERMISSION_BY_ID WRITE b.txt=2' }).waitFor({ timeout: 60000 });
  await page.locator('#events', { hasText: 'session started' }).waitFor();

  for (const hash of ['#/inbox', '#/approvals', '#/attention']) {
    await go(hash);
    await page.getByRole('heading', { level: 1, name: 'Needs you' }).waitFor();
  }

  // A reply written as markdown renders as elements, never as HTML: its
  // emphasis, headings, nested lists, table, web link and code; a JSON
  // fence as its keys and values. A relative link target stays text.
  await nextLine();
  await go('#/b/app/report');
  const rich = page.locator('#convo .turn .agent .rich').first();
  await rich.waitFor({ timeout: 60000 });
  await rich.locator('strong', { hasText: 'two files' }).waitFor();
  await rich.locator('h3.md-h', { hasText: 'What changed' }).waitFor();
  await rich.locator('ul > li > ul > li', { hasText: 'nested note' }).waitFor();
  await rich.locator('li code', { hasText: 'src/record.rs' }).first().waitFor();
  await rich.locator('table.md-table td.num', { hasText: '21.7' }).waitFor();
  const webLink = rich.locator('a', { hasText: 'the logfmt spec' });
  if (await webLink.getAttribute('href') !== 'https://brandur.org/logfmt' || await webLink.getAttribute('rel') !== 'noopener noreferrer') throw new Error('a web link is not a safe link');
  if (await rich.locator('a', { hasText: 'a local file' }).count()) throw new Error('a relative target became a link');
  const json = rich.locator('figure.code-block.is-json');
  await json.locator('.code-head', { hasText: 'JSON' }).waitFor();
  await json.locator('dl.json-facts dt', { hasText: 'summary' }).waitFor();
  await json.locator('dl.json-facts dd', { hasText: 'Two files changed.' }).waitFor();
  await json.locator('dl.json-facts dd .j-num', { hasText: '2' }).waitFor();
  if (await json.locator('details[open]').count()) throw new Error('the JSON text is open before it is asked for');
  await json.locator('details > summary').click();
  await json.locator('pre .j-key', { hasText: '"summary"' }).waitFor();
  const shell = rich.locator('figure.code-block:not(.is-json)');
  await shell.locator('.code-head', { hasText: 'sh' }).waitFor();
  await shell.locator('pre code', { hasText: 'cargo test --test parser' }).waitFor();
  await shell.getByRole('button', { name: 'Copy sh', exact: true }).waitFor();
  if (!(await page.locator('#convo .agent-head .ai-tag').count())) throw new Error('the reply is not labelled as the agent\'s');
  const convoText = await page.locator('#convo').innerText();
  if (convoText.includes('```') || convoText.includes('**')) throw new Error('markdown syntax is shown as text');
  await noOverflow('a rendered reply');
  say('RENDERED');

  // Each turn's receipt shows what that turn cost, as the engine recorded
  // it; the strip keeps the branch's total.
  await nextLine();
  await go('#/b/app/costly');
  const receipts = page.locator('#convo .turn .outcome');
  await receipts.filter({ hasText: '$0.25' }).waitFor({ timeout: 60000 });
  await receipts.filter({ hasText: '$0.50' }).waitFor({ timeout: 60000 });
  if (await receipts.filter({ hasText: '$0.75' }).count()) throw new Error('a receipt shows the session total, not its turn');
  await page.locator('.strip-meta', { hasText: '$0.75' }).waitFor();
  say('COSTED');

  // A turn parked on an approval: the branch itself says so on the board
  // and in the lane, the deck shows the ask first, Later sends it to the
  // back and it comes round, Enter allows after an undo window, and Undo
  // within the window sends nothing.
  await nextLine();
  await go('#/');
  await page.locator('#main .board-row.board-parked[data-branch="asker"] .rail .wait').waitFor({ timeout: 60000 });
  await page.locator('#lane-needs a', { hasText: 'asker' }).filter({ hasText: 'waiting for you' }).waitFor();
  if (await page.title() !== '(1) Branchyard') throw new Error(`the title counts ${await page.title()}, not (1)`);
  await go('#/attention');
  const card = page.locator('#main .deck-card.deck-approval');
  await card.waitFor({ timeout: 60000 });
  await card.locator('h2', { hasText: 'asker wants to' }).waitFor();
  if (await page.locator('#main .deck-card').count() !== 1) throw new Error('the deck shows more than one card');
  if (!(await page.locator('#main .deck-count', { hasText: '1 of 3' }).count())) throw new Error(`the parked agent is not first: ${await page.locator('#main .deck-count').innerText()}`);
  await card.focus();
  await page.keyboard.press('s');
  // Sent to the back, the older ready branches come first and the ask
  // waits in the bay, which brings it back.
  await page.locator('#main .deck-card.deck-merge', { hasText: 'first is ready to merge' }).waitFor();
  await page.locator('#main .bay-item[data-decision^="approval:asker:"]').click();
  await page.locator('#main .deck-card.deck-approval').waitFor();
  await page.locator('#main .deck-card').focus();
  await page.keyboard.press('Enter');
  const held = page.locator('.bay-held', { hasText: 'asker: allowing' });
  await held.waitFor();
  await held.getByRole('button', { name: /^Undo/ }).click();
  await held.waitFor({ state: 'detached' });
  if (await page.locator('.toast', { hasText: 'Allowed' }).count()) throw new Error('an undone allow was sent');
  await page.locator('#main .deck-card.deck-approval').waitFor();
  await page.locator('#main .deck-card').focus();
  await page.keyboard.press('Enter');
  await page.locator('.bay-held', { hasText: 'asker: allowing' }).waitFor();
  await page.locator('.toast', { hasText: 'Allowed: asker may' }).waitFor({ timeout: 30000 });
  await page.locator('#lane-ready a', { hasText: 'asker' }).waitFor({ timeout: 60000 });
  // Ready, it is a merge card behind the older ready branches; the bay
  // brings it to the front.
  await page.locator('#main .bay-item[data-decision^="merge:asker:"]').click({ timeout: 60000 });
  await page.locator('#main .deck-card.deck-merge', { hasText: 'asker is ready to merge' }).waitFor({ timeout: 60000 });
  await page.locator('#main .deck-card .deck-files li', { hasText: 'd.txt' }).waitFor({ timeout: 60000 });
  await noOverflow('the deck');
  say('DECIDED');

  // A child asked the branch that delegated it which parser to use and
  // waits for the answer. It is a parked agent: the tab title counts it as
  // it arrives, the bay lists it ahead of the ready branches, and, taken
  // in the server's order after a reload, it is the deck's first card.
  // Enter in the card's answer box sends the text itself (an answer is a
  // sentence, not a held allow); nothing is left to undo.
  await nextLine();
  await page.waitForFunction(() => document.title === '(1) Branchyard', null, { timeout: 60000 });
  await page.waitForFunction(() => {
    const first = document.querySelector('#main .bay-item');
    return first && first.dataset.decision.startsWith('question:kid:');
  }, null, { timeout: 60000 });
  const bayFirst = page.locator('#main .bay-item').first();
  await bayFirst.waitFor({ timeout: 60000 });
  if (!(await bayFirst.getAttribute('data-decision') || '').startsWith('question:kid:')) throw new Error(`the bay does not list the question first: ${await bayFirst.innerText()}`);
  await page.reload({ waitUntil: 'domcontentloaded' });
  const question = page.locator('#main .deck-card.deck-question');
  await question.waitFor({ timeout: 60000 });
  if (!(await question.evaluate((el) => el.classList.contains('deck-question')))) throw new Error(`the first card is not the question: ${await question.innerText()}`);
  if (await page.locator('#main .deck-card').count() !== 1) throw new Error('the deck shows more than one card');
  await question.locator('h2', { hasText: 'kid asks: Which parser?' }).waitFor();
  await question.locator('.deck-kind', { hasText: 'an agent is parked until you answer' }).waitFor();
  await question.locator('.deck-line', { hasText: 'to boss' }).waitFor();
  await page.locator('#lane-needs a', { hasText: 'kid' }).filter({ hasText: 'question' }).waitFor();
  if (!(await page.locator('#main .deck-count', { hasText: /^1 of \d+/ }).count())) throw new Error(`the deck is not counted from the question: ${await page.locator('#main .deck-count').innerText()}`);
  if (await page.title() !== '(1) Branchyard') throw new Error(`the title counts ${await page.title()}, not (1)`);
  await noOverflow('the question card');
  say('ASKED');

  await nextLine();
  const answerBox = question.getByLabel('Answer as boss');
  await answerBox.fill('Use the second parser.');
  await answerBox.press('Enter');
  await page.locator('.toast', { hasText: 'Answered kid.' }).waitFor({ timeout: 30000 });
  await page.locator('#main .deck-card.deck-question').waitFor({ state: 'detached', timeout: 30000 });
  if (await page.locator('.bay-held').count()) throw new Error('an answer to a question was held for an undo');
  await page.locator('#main .bay-item[data-decision^="question:"]').waitFor({ state: 'detached' });
  await page.waitForFunction(() => document.title === 'Branchyard', null, { timeout: 60000 });
  if (await page.locator('#main .deck-card.deck-question').count()) throw new Error('the answered question is still in the deck');
  say('ANSWERED');

  // BY_SCREENSHOTS=DIR saves desktop and phone views in both themes, for a look.
  if (shots) {
    const views = [
      ['home', '#/', () => page.getByRole('heading', { level: 1, name: 'app', exact: true })],
      ['attention', '#/attention', () => page.getByRole('heading', { level: 1, name: 'Needs you' })],
      ['branch', '#/b/app/first', () => page.locator('pre.diff')],
    ];
    await page.setViewportSize({ width: 1440, height: 900 });
    for (const scheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const [name, hash, ready] of views) {
        await go(hash);
        await ready().waitFor();
        await page.screenshot({ path: `${shots}/desktop-${name}-${scheme}.png` });
      }
    }
    const phone = [
      ['branch', '#/b/app/first', () => page.locator('pre.diff')],
      ['branches', '#/', () => item],
      ['inbox', '#/inbox', () => page.getByRole('heading', { level: 1, name: 'Needs you' })],
      ['queue', '#/queue', () => page.getByRole('heading', { level: 1, name: 'Queue and stats' })],
      ['settings', '#/settings', () => page.getByRole('heading', { level: 1, name: 'Settings' })],
    ];
    await page.setViewportSize({ width: 390, height: 844 });
    for (const scheme of ['light', 'dark']) {
      await page.emulateMedia({ colorScheme: scheme });
      for (const [name, hash, ready] of phone) {
        await go(hash);
        await ready().waitFor();
        await page.screenshot({ path: `${shots}/${name}-${scheme}.png`, fullPage: name === 'branch' });
      }
    }
    await page.emulateMedia({ colorScheme: 'light' });
  }

  // The ground plan: another open branch writes one of first's files. The
  // branch page's Ground names it beside that file and says "only here"
  // beside the other; first's merge card says which file is in two hands,
  // and a card whose files nobody else holds says nothing.
  await nextLine();
  await page.setViewportSize({ width: 1280, height: 900 });
  await go('#/b/app/first');
  await page.getByRole('tab', { name: /^Ground/ }).click();
  await page.locator('#ground li[data-path="a.txt"]', { hasText: 'also in rival' }).waitFor({ timeout: 60000 });
  await page.locator('#ground li[data-path="b.txt"]', { hasText: 'only here' }).waitFor();
  await page.locator('#ground-count', { hasText: '1' }).waitFor();
  await page.locator('#ground a', { hasText: 'rival' }).click();
  await page.locator('#strip h1', { hasText: 'rival' }).waitFor();
  await page.getByRole('tab', { name: /^Ground/ }).click();
  await page.locator('#ground li[data-path="a.txt"]', { hasText: 'also in first' }).waitFor({ timeout: 30000 });
  await go('#/attention');
  // first is the front card unless a parked agent or an older ready branch
  // precedes it; then the bay brings it forward.
  await page.locator('#main .deck-card').waitFor({ timeout: 60000 });
  if (!(await page.locator('#main .deck-card.deck-merge', { hasText: 'first is ready to merge' }).count())) {
    await page.locator('#main .bay-item[data-decision^="merge:first:"]').click({ timeout: 60000 });
  }
  await page.locator('#main .deck-card.deck-merge .deck-ground', { hasText: 'a.txt also in rival' }).waitFor({ timeout: 60000 });
  if (await page.locator('#main .deck-card .deck-ground', { hasText: 'b.txt' }).count()) throw new Error('a file only first holds is named as in two hands');
  await page.locator('#main .bay-item[data-decision^="merge:asker:"]').click();
  await page.locator('#main .deck-card .deck-files li', { hasText: 'd.txt' }).waitFor({ timeout: 60000 });
  if (await page.locator('#main .deck-card .deck-ground').count()) throw new Error('a card whose files nobody else holds names some');
  await noOverflow('the deck with the ground line');
  say('GROUND');

  // Queued work that no worker can claim is a problem. Nothing on the
  // stream announces it, so the page reads it when it loads: home lists it
  // above the tiles, with how long it has stood, its error and what to do.
  const stuck = (await nextLine()).split(' ')[1];
  await go('#/');
  await page.reload({ waitUntil: 'domcontentloaded' });
  const strip = page.locator('#main section[aria-label="Problems"]');
  const problem = strip.locator('li.problem', { hasText: `operation ${stuck} on app` });
  await problem.waitFor({ timeout: 60000 });
  if (await strip.locator('li.problem').count() !== 1) throw new Error(`the strip lists ${await strip.locator('li.problem').count()} problems, not 1`);
  await problem.locator('.problem-error', { hasText: 'gpu' }).waitFor();
  await problem.locator('.problem-next', { hasText: 'it claims the operation' }).waitFor();
  await problem.locator('.problem-next code', { hasText: '--label gpu' }).waitFor();
  await problem.locator('.problem-meta time[data-at]').waitFor();
  await page.locator('#main .tiles').waitFor();
  const [stripBox, tilesBox] = await Promise.all([strip.boundingBox(), page.locator('#main .tiles').boundingBox()]);
  if (stripBox.y + stripBox.height > tilesBox.y) throw new Error(`the problems strip is not above the tiles: ${JSON.stringify([stripBox, tilesBox])}`);
  await noOverflow('the problems strip');
  say('PROBLEM');

  await go('#/b/app/first');
  await page.locator('#strip').getByRole('button', { name: 'Merge', exact: true }).click();
  await page.locator('#confirm-ok').click();
  await page.locator('.toast', { hasText: 'Merged into' }).waitFor({ timeout: 60000 });
  await page.locator('#strip .state-merged svg.check').waitFor();
  await page.locator('#lane-done').waitFor({ state: 'attached' });
  if (await page.locator('#lane-ready a', { hasText: /^first/ }).count()) throw new Error('a merged branch stayed in Ready');
  await page.getByRole('tab', { name: /^Ground/ }).click();
  await page.locator('#ground', { hasText: 'Merged, this branch holds no files.' }).waitFor();

  // A token can expire between a failed list request and its retry.
  let listAttempts = 0;
  await page.route('**/v1/repos/app/branches', async (route) => {
    listAttempts += 1;
    const status = listAttempts === 1 ? 503 : 401;
    await route.fulfill({ status, contentType: 'application/json', body: JSON.stringify({ error: {
      code: status === 503 ? 'unavailable' : 'unauthorized',
      message: status === 503 ? 'Fixture server temporarily unavailable.' : 'Fixture session expired.',
    } }) });
  });
  await page.goto(`${process.env.BY_URL}/app/`, { waitUntil: 'domcontentloaded' });
  await page.getByRole('heading', { name: 'Could not load branches' }).waitFor();
  await page.getByRole('button', { name: 'Retry loading branches' }).click();
  await page.getByRole('heading', { name: 'Connect to Branchyard' }).waitFor();
  await page.getByLabel('Server token', { exact: true }).fill('new-token-fixture-only');
  if (await page.getByRole('button', { name: 'Connect to server', exact: true }).isDisabled()) throw new Error('expired-token recovery left connection disabled');
  const signedOut = await page.evaluate(() => sessionStorage.getItem('branchyard.token') === null && document.getElementById('sidebar').hidden);
  if (!signedOut || listAttempts !== 2) throw new Error('retry did not clear the expired session');

  const csp = problems.filter((p) => /Content.Security.Policy|Refused to/i.test(p));
  if (csp.length) throw new Error(`policy violations: ${csp.join(' | ')}`);
  const errors = problems.filter((p) => !/Failed to load resource/.test(p));
  if (errors.length) throw new Error(`page errors: ${errors.join(' | ')}`);
  say('DONE');
  await browser.close();
  process.exit(0);
})().catch(async (e) => {
  process.stderr.write(`${e.stack || e}\n`);
  if (shown) process.stderr.write(`the page showed:\n${await shown().catch(() => '?')}\n`);
  if (browser) await browser.close().catch(() => {});
  process.exit(1);
});
