// Drives the console's Trajectory page in headless Chromium for
// tests/console_trajectory_browser.rs, which runs the server and talks to
// this script over stdin and stdout, a line at a time. argv[2] is the
// path of trajectory.js, whose pure functions are checked under Node first.
//
//   script: MODEL        the timeline model: folding the stream gives what the
//                        server's reading gives, idle gaps fold, fans group
//   script: LOADED       the page opened a branch with three turns
//   script: OVERVIEW     a row per turn; startup, tool calls, a steer; two
//                        helper lanes under the turn that spawned them, one
//                        failed; no event page was read
//   script: LAZY         a step read exactly its own events from the event page
//   script: KEYBOARD     arrows, Enter, Escape, zoom keys; one tab stop
//   script: LANES        a lane expands and collapses, naming its failure
//   script: COMPARE      the helpers side by side, one with the judge's score
//   script: PERMISSION   another branch's denied permission: a wait, its request
//                        and its answer read on demand
//   script: SYNTHETIC    exact geometry on synthetic timelines
//   script: LIVE READY
//   test:   (a fourth turn is recorded) "live"
//   script: LIVE         the new turn drew from the stream, with no second read
//   script: VIEWPORTS    1440 and 390 pixels: no page overflow; light and dark
//   script: DONE
//
// PLAYWRIGHT names the playwright module; BY_URL the server; BY_CODE a
// pairing code; BY_SCREENSHOTS a directory for screenshots.
'use strict';

const { chromium } = require(process.env.PLAYWRIGHT);
const readline = require('readline');
const assert = require('assert');

const lines = readline.createInterface({ input: process.stdin });
const waiting = [];
const arrived = [];
let closed = false;
lines.on('line', (line) => { const w = waiting.shift(); if (w) w.resolve(line); else arrived.push(line); });
lines.on('close', () => { closed = true; for (const w of waiting.splice(0)) w.reject(new Error('the test controller closed stdin')); });
const nextLine = () => (arrived.length ? Promise.resolve(arrived.shift())
  : closed ? Promise.reject(new Error('the test controller closed stdin'))
    : new Promise((resolve, reject) => waiting.push({ resolve, reject })));
const say = (text) => { process.stderr.write(`[page] ${text}\n`); process.stdout.write(`${text}\n`); };
// A hung page ends the test with where it was, not a stuck build.
let where = 'starting';
setTimeout(() => { process.stderr.write(`[page] still at ${where} after four minutes\n`); process.exit(3); }, 240000).unref();
const shots = process.env.BY_SCREENSHOTS;
const eq = (actual, expected, what) => assert.deepStrictEqual(actual, expected, `${what}: ${JSON.stringify(actual)} is not ${JSON.stringify(expected)}`);
const ok = (value, what) => assert.ok(value, what);

// ---------------------------------------------------------------
// The model, under Node: no browser, no server.

function checkModel(path) {
  const { model } = require(path);
  const T = 1_000_000;
  // The same log read two ways: folded event by event, as the page keeps a
  // lane current from the stream, and as one timeline, as the server's
  // `Timeline::of` reads it. They agree, and folding an event twice
  // changes nothing.
  const events = [
    [T + 0, { startup: { provider: 'local', path: 'cold', phases: { queued: 0, handshake_done: 80, first_event: 120 } } }],
    [T + 1, { prompt: 'go' }],
    [T + 10, { harness: { type: 'tool_started', turn: 1, call_id: 'a', name: 'Read' } }],
    [T + 20, { harness: { type: 'permission_requested', turn: 1, request: { key: 'k', tool: 'write', input: {} } } }],
    [T + 50, { decision: { tool: 'write', allowed: false, message: 'no', source: 'asked' } }],
    [T + 60, { steered: { id: 1, by: 'p', text: 'x' } }],
    [T + 70, { harness: { type: 'usage_observed', turn: 1, usage: { cumulative: true, input_tokens: 10, output_tokens: 5 } } }],
    [T + 80, { harness: { type: 'usage_observed', turn: 1, usage: { cumulative: true, input_tokens: 30, output_tokens: 10 } } }],
    [T + 90, { delegation: { tool: 'spawn', branch: 'kid', outcome: 'ok', refused: false } }],
    [T + 95, { delegation: { tool: 'spawn', branch: '', outcome: 'too deep', refused: true } }],
    [T + 100, { turn_cost: { cost_usd: 0.25 } }],
    [T + 110, { status: { state: 'ready' } }],
    [T + 200, { prompt: 'again' }],
    [T + 210, { status: { state: 'failed', reason: 'boom' } }],
  ];
  const shape = (lane) => JSON.stringify(lane.turns.map((t) => [t.start, t.end, t.tools.length, t.marks.length, t.tokens, t.cost]));
  const folded = model.laneOf('x', { turns: [] });
  const twice = model.laneOf('x', { turns: [] });
  for (const [ms, a] of events) { model.fold(folded, a, ms); model.fold(twice, a, ms); model.fold(twice, a, ms); }
  eq(shape(twice), shape(folded), 'an event seen twice changes nothing');
  const turn = folded.turns[0];
  eq([turn.start, turn.end, turn.outcome.state, turn.cost, turn.tokens], [T + 1, T + 110, 'ready', 0.25, 40], 'the first turn folded');
  eq(turn.marks.map((m) => [m.kind, m.label, m.ok]), [['permission', 'write', null], ['decision', 'write', false], ['steer', null, null], ['delegation', 'kid', true], ['delegation', null, false]], 'marks');
  eq(turn.startup.provider, 'local', 'a startup recorded before the prompt');
  eq(folded.turns[1].outcome.state, 'failed', 'the second turn failed');
  // The wire's reading: the same lane from a timeline.
  const wire = model.laneOf('x', {
    turns: [{
      seq: 2, started_at_ms: T + 1, ended_at_ms: T + 110, outcome: { state: 'ready' }, cost_usd: 0.25, tokens: 40,
      tools: [{ at_ms: T + 10, name: 'Read', call_id: 'a', seq: 3 }],
      marks: [
        { at_ms: T + 20, seq: 4, kind: 'permission', label: 'write' }, { at_ms: T + 50, seq: 5, kind: 'decision', label: 'write', ok: false },
        { at_ms: T + 60, seq: 6, kind: 'steer' }, { at_ms: T + 90, seq: 9, kind: 'delegation', label: 'kid', ok: true },
        { at_ms: T + 95, seq: 10, kind: 'delegation', ok: false },
      ],
    }],
  });
  eq(wire.turns[0].marks.map((m) => [m.kind, m.label, m.ok]), turn.marks.map((m) => [m.kind, m.label, m.ok]), 'the wire and the fold agree on marks');
  // A turn's time is tiled: model, tool, wait, tool again, model.
  const { segs, steps } = model.stepsOf(wire, wire.turns[0], T + 500);
  eq(segs.map((s) => [s.kind, s.from - T, s.to - T]), [['model', 1, 10], ['tool', 10, 20], ['wait', 20, 50], ['tool', 50, 110]], 'segments');
  eq(steps.filter((s) => s.type === 'wait').map((s) => s.seg.decision.ok), [false], 'the wait names its answer');
  ok(steps.every((s, i) => i === 0 || steps[i - 1].at <= s.at), 'steps in time order');
  const split = model.split(wire, wire.turns[0], T + 500);
  eq([split.tools, split.wait, split.model], [10 + 60, 30, 9], 'where the time went');
  // A turn still open ends at now; a wait with no answer is open.
  const open = model.laneOf('o', { turns: [{ seq: 1, started_at_ms: T, tools: [], marks: [{ at_ms: T + 5, seq: 2, kind: 'permission', label: 'rm' }] }] });
  const o = model.stepsOf(open, open.turns[0], T + 100);
  eq(o.segs.map((s) => [s.kind, !!s.open, s.to - T]), [['model', false, 5], ['wait', true, 100]], 'an open wait runs to now');
  // The axis: a gap longer than a quarter of the work folds to a break.
  const scale = model.buildScale([[T, T + 10_000], [T + 7_200_000, T + 7_210_000]], T + 8_000_000, true);
  ok(scale.pieces.some((p) => p.brk), 'an idle gap is folded');
  ok(scale.total < 40_000, `the folded axis is short: ${scale.total}`);
  eq(scale.toV(T + 5_000), 5_000, 'the first piece is real time');
  ok(scale.toV(T + 7_200_000) < 12_000, 'the second piece follows the break closely');
  const real = model.buildScale([[T, T + 10_000], [T + 7_200_000, T + 7_210_000]], T + 8_000_000, false);
  eq(real.total, 7_210_000, 'unfolded, the axis is real time');
  eq(model.buildScale([[T, T + 60_000], [T + 70_000, T + 80_000]], T, true).pieces.some((p) => p.brk), false, 'a short gap stays');
  eq(model.niceStep(84 / 0.05), 2000, 'a nice step');
  eq([model.fmtDur(450), model.fmtDur(1500), model.fmtDur(65_000), model.fmtDur(3_725_000)], ['450 ms', '1.5 s', '1m 05s', '1h 02m'], 'durations');
  eq(model.fmtTokens(1234), '1.2k', 'tokens');
  // A fan: `by fan` starts top-level branches named <name>-<harness>, one prompt.
  const b = (name, harness, extra) => [name, { name, harness, profile: harness, prompt: 'p', children: [], depth: 0, created_at: 1, ...extra }];
  const branches = new Map([b('job-claude-code', 'claude-code'), b('job-codex', 'codex'), b('job-codex-2', 'codex'), b('other-codex', 'codex', { prompt: 'q' }), b('solo', 'codex')]);
  eq(model.fanGroup(branches, 'job-codex'), ['job-claude-code', 'job-codex', 'job-codex-2'], 'a fan group');
  eq(model.fanGroup(branches, 'solo'), [], 'no group');
  const family = new Map([['p', { name: 'p', children: ['a', 'b'] }], ['a', { name: 'a', parent: 'p', children: [] }], ['b', { name: 'b', parent: 'p', children: [] }]]);
  eq(model.fanGroup(family, 'a'), ['a', 'b'], 'siblings');
  say('MODEL');
}

// ---------------------------------------------------------------
// The page

(async () => {
  checkModel(process.argv[2]);
  const browser = await chromium.launch();
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
  context.setDefaultTimeout(30000);
  const page = await context.newPage();
  const problems = [];
  page.on('console', (m) => { if (m.type() === 'error') problems.push(m.text()); });
  page.on('pageerror', (e) => problems.push(String(e)));
  const requests = [];
  page.on('request', (r) => requests.push(r.url()));
  const count = (re) => requests.filter((u) => re.test(u)).length;
  const timelines = new Map();
  page.on('response', async (r) => {
    const m = /\/branches\/([^/]+)\/timeline$/.exec(r.url());
    if (m && r.ok()) { try { timelines.set(m[1], await r.json()); } catch (_) { /* a reload in flight */ } }
  });
  const url = process.env.BY_URL;
  const go = (hash) => page.evaluate((h) => { location.hash = h; }, hash);
  const noOverflow = async (what) => {
    const [scroll, inner] = await page.evaluate(() => [document.documentElement.scrollWidth, innerWidth]);
    if (scroll > inner) throw new Error(`${what} overflows at ${inner}px (${scroll})`);
  };
  const active = () => page.evaluate(() => (document.activeElement && document.activeElement.dataset ? document.activeElement.dataset.step || null : null));
  // A step opened as the page's own click handler gets it. At the fitted
  // zoom calls a few milliseconds long sit under one another within a
  // pixel or two (the page clusters them, and zooming separates them), so
  // a pointer's centre can land on a neighbour.
  const open = (locator) => locator.dispatchEvent('click');
  const rows = (lane, turn) => page.locator(`.tj-turn[data-lane="${lane}"]${turn ? `[data-turn="${turn}"]` : ''}`);

  // The helper's timeline gains the judge's score on the way, so the
  // comparison has one to show; the server holds none (a judge needs a
  // model or a check run).
  await page.route('**/branches/helper/timeline', async (route) => {
    const response = await route.fetch();
    const json = await response.json();
    const last = json.turns[json.turns.length - 1];
    last.marks = [...(last.marks || []), { at_ms: last.ended_at_ms || last.started_at_ms, seq: 0, kind: 'judged', score: 87.5 }];
    await route.fulfill({ response, json });
  });

  await page.goto(`${url}/app/#pair=${process.env.BY_CODE}`);
  await page.locator('#main h1').first().waitFor({ timeout: 60000 });
  await page.locator('.board-row[data-branch="lead"]').waitFor({ state: 'attached', timeout: 60000 });
  // A branch with no changes is done, and the done group is folded.
  if (!(await page.locator('.board-row[data-branch="lead"]').isVisible())) await page.locator('.board-done-head').click();
  await page.locator('.board-row[data-branch="lead"]').waitFor({ timeout: 60000 });
  // The board's row links to the trajectory.
  const link = page.locator('.board-row[data-branch="lead"] a.board-traj');
  await link.waitFor();
  eq(await link.getAttribute('aria-label'), 'Trajectory of lead', 'the board row\'s link');
  requests.length = 0;
  await link.click();
  await page.getByRole('heading', { level: 1, name: 'Trajectory: lead' }).waitFor();
  await page.locator('.tj-host[data-ready="true"]').waitFor({ timeout: 60000 });
  await page.locator('.tj-child[data-lane="helper"]').waitFor({ timeout: 60000 });
  await page.locator('.tj-child[data-lane="flaky"]').waitFor();
  // The branch page links to it too.
  await go('#/b/app/lead');
  await page.locator('a[data-trajectory="lead"]').waitFor();
  requests.length = 0;
  await go('#/trajectory/app/lead');
  await page.locator('.tj-host[data-ready="true"]').waitFor();
  await page.locator('.tj-turn[data-lane="lead"][data-turn="3"]').waitFor();
  await page.locator('.tj-child[data-lane="flaky"]').waitFor();
  say('LOADED');

  where = 'overview';
  // ---- overview ----
  eq(await rows('lead').count(), 3, 'a row for each of the lead\'s turns');
  eq(await page.locator('[data-stat="turns"]').innerText(), '3', 'the summary counts the turns');
  for (const stat of ['harness', 'duration', 'tool-calls', 'outcome', 'startup', 'tokens', 'cost', 'effort', 'model']) {
    ok(await page.locator(`[data-stat="${stat}"]`).count(), `the summary has ${stat}`);
  }
  ok((await page.locator('[data-stat="startup"]').innerText()).startsWith('local'), 'the summary names the startup provider');
  ok(await page.locator('.tj-timeuse .tj-split').getAttribute('aria-label'), 'the time split has a text alternative');
  // Startup in every turn; its label names the provider.
  eq(await page.locator('.tj-turn[data-lane="lead"] [data-kind="startup"]').count(), 3, 'each turn has its startup');
  ok((await page.locator('.tj-turn[data-turn="1"] [data-kind="startup"]').getAttribute('aria-label')).startsWith('Startup on local'), 'the startup band is labelled');
  // Turn 1's calls: three bars, or one cluster when they fall in the same few pixels.
  const calls = await rows('lead', 1).locator('[data-kind="tool"]').evaluateAll((els) => els.map((e) => e.getAttribute('aria-label')));
  ok(calls.length >= 1 && calls.length <= 3, `turn 1 draws its calls: ${JSON.stringify(calls)} from ${JSON.stringify(timelines.get('lead') && timelines.get('lead').turns[0])}; ${await rows('lead', 1).evaluate((e) => e.outerHTML.slice(0, 1800))}`);
  ok(/Read|\d+ tool calls/.test(calls.join(' ')), 'turn 1 names its first call or counts them');
  // Turn 2 delegated twice; the helpers' lanes sit between turn 2 and turn 3.
  eq(await rows('lead', 2).locator('[data-kind="delegation"]').count(), 2, 'two delegations in turn 2');
  const order = await page.evaluate(() => {
    const all = Array.from(document.querySelectorAll('.tj-row'));
    const at = (sel) => all.indexOf(document.querySelector(sel));
    return [at('.tj-turn[data-lane="lead"][data-turn="2"]'), at('.tj-child[data-lane="helper"]'), at('.tj-child[data-lane="flaky"]'), at('.tj-turn[data-lane="lead"][data-turn="3"]')];
  });
  ok(order[0] >= 0 && order[0] < order[1] && order[1] < order[2] && order[2] < order[3], `the helpers nest under turn 2: ${order}`);
  // The failed helper's bar is marked by a cross and a hatch, not only a colour.
  const flaky = page.locator('.tj-child[data-lane="flaky"] .tj-sum');
  ok((await flaky.innerText()).includes('✕'), 'the failed helper carries a cross');
  ok((await flaky.getAttribute('class')).includes('tj-bad'), 'and the failed class');
  ok((await flaky.getAttribute('aria-label')).includes('failed'), 'and says it failed');
  const helperLane = await page.locator('.tj-child[data-lane="helper"] .tj-lane-meta').first().innerText();
  ok(helperLane.includes('gemini-cli'), `a lane names its harness: ${helperLane}`);
  eq(await page.locator('.tj-child[data-lane="helper"] .tj-score').innerText(), 'score 88', 'a lane shows the judge\'s score when there is one');
  eq(await rows('lead', 3).locator('[data-kind="steer"]').count(), 1, 'a steer in turn 3');
  // The overview read timelines only: the lead's, and each helper's.
  eq(count(/\/branches\/lead\/timeline$/), 1, 'the lead\'s timeline was read once');
  eq(count(/\/branches\/helper\/timeline$/), 1, 'the helper\'s timeline was read once');
  eq(count(/\/branches\/flaky\/timeline$/), 1, 'the other helper\'s timeline was read once');
  eq(count(/event-page/), 0, 'the overview read no event page');
  say('OVERVIEW');

  where = 'lazy detail';
  // ---- lazy detail ----
  const lead = timelines.get('lead');
  ok(lead && lead.turns.length === 3, 'the timeline reached the page');
  eq(await page.locator('.tj-detail .tj-detail-body').count(), 0, 'no detail before a step is opened');
  const first = rows('lead', 1).locator('[data-kind="tool"]').first();
  await open(first);
  await page.locator('.tj-detail[data-step]').waitFor();
  await page.locator('.tj-detail .tj-events li').first().waitFor();
  eq(count(/event-page/), 1, 'one step read one page');
  const request = requests.find((u) => /event-page/.test(u));
  const want = lead.turns[0].tools[0].seq - 1;
  ok(request.includes(`/branches/lead/event-page?cursor=${want}&`), `the step asked for its own events (${want}): ${request}`);
  const detail = await page.locator('.tj-detail').innerText();
  ok(/Tool call: Read|\d+ tool calls: Read/.test(detail), `the detail names the call: ${detail.slice(0, 120)}`);
  ok(/tool Read \(call tool-1\)/.test(detail), 'and shows the call\'s own event');
  ok(await page.locator('.tj-detail .tj-raw summary').count(), 'the raw events are one click away');
  // The raw events link back to the branch.
  ok((await page.locator('.tj-detail a[href^="#/b/app/lead"]').count()) > 0, 'the detail links to the branch');
  // The first turn's prompt.
  await open(rows('lead', 1).locator('[data-kind="prompt"]'));
  await page.locator('.tj-detail .tj-events li', { hasText: 'plan the work' }).waitFor();
  // The startup band reads from the timeline itself.
  await open(rows('lead', 1).locator('[data-kind="startup"]'));
  await page.locator('.tj-detail table.tj-table').waitFor();
  eq(count(/event-page/), 2, 'a startup step reads no event page');
  say('LAZY');

  where = 'keyboard';
  // ---- keyboard ----
  const stops = await page.locator('.tj-scroll [data-step][tabindex="0"]').count();
  eq(stops, 1, 'one tab stop in the waterfall');
  const stepIds = await page.locator('.tj-scroll [data-step]').evaluateAll((els) => els.map((e) => e.dataset.step));
  await page.locator('.tj-scroll [data-step]').first().focus();
  eq(await active(), stepIds[0], 'the first step takes focus');
  await page.keyboard.press('ArrowRight');
  eq(await active(), stepIds[1], 'ArrowRight moves to the next step');
  await page.keyboard.press('ArrowRight');
  await page.keyboard.press('ArrowLeft');
  eq(await active(), stepIds[1], 'ArrowLeft moves back');
  const row0 = await page.evaluate(() => document.activeElement.closest('.tj-row').dataset.turn);
  await page.keyboard.press('ArrowDown');
  const row1 = await page.evaluate(() => { const r = document.activeElement.closest('.tj-row'); return r.dataset.turn || r.dataset.lane; });
  ok(row1 !== row0, `ArrowDown moves to the next row: ${row0} then ${row1}`);
  await page.keyboard.press('Home');
  eq(await active(), stepIds[0], 'Home goes to the first step');
  await page.keyboard.press('End');
  eq(await active(), stepIds[stepIds.length - 1], 'End goes to the last');
  await page.keyboard.press('Home');
  await page.keyboard.press('Enter');
  await page.locator(`.tj-detail[data-step="${stepIds[0]}"]`).waitFor();
  eq(await page.locator(`.tj-scroll [data-step="${stepIds[0]}"]`).getAttribute('aria-pressed'), 'true', 'Enter opens a step');
  await page.keyboard.press('Escape');
  await page.locator('.tj-detail', { hasText: 'Select a step' }).waitFor();
  const zoomIs = (text) => page.locator(`.tj-zoomval:text-is("${text}")`).waitFor();
  await zoomIs('100%');
  await page.keyboard.press('+');
  await zoomIs('160%');
  await page.keyboard.press('-');
  await zoomIs('100%');
  await page.keyboard.press('+');
  await zoomIs('160%');
  await page.keyboard.press('0');
  await zoomIs('100%');
  await page.getByRole('button', { name: 'Zoom to turn 1', exact: true }).click();
  await page.waitForFunction(() => document.querySelector('.tj-zoomval').textContent !== '100%');
  await page.getByRole('button', { name: 'Fit', exact: true }).click();
  // Labels: every step says what it is; the waterfall and its controls are named.
  const unlabeled = await page.locator('.tj-scroll [data-step]').evaluateAll((els) => els.filter((e) => !(e.getAttribute('aria-label') || '').trim()).length);
  eq(unlabeled, 0, 'every step has a label');
  ok(await page.locator('.tj-scroll[role="group"][aria-label]').count(), 'the waterfall is a named group');
  eq(await page.getByRole('button', { name: 'Zoom in', exact: true }).count(), 1, 'the zoom buttons are named');
  ok(await page.locator('.tj-detail[aria-live="polite"]').count(), 'the detail is a live region');
  ok(await page.locator('.tj-listing summary').count(), 'the steps are also a list');
  await page.locator('.tj-listing summary').click();
  await page.locator('.tj-steps li').first().waitFor();
  ok((await page.locator('.tj-steps li').count()) > 10, 'the list names the steps');
  await page.locator('.tj-listing summary').click();
  say('KEYBOARD');

  where = 'lanes';
  // ---- lanes ----
  const toggle = page.getByRole('button', { name: 'Expand flaky', exact: true });
  eq(await toggle.getAttribute('aria-expanded'), 'false', 'a lane starts collapsed');
  await toggle.click();
  await rows('flaky').first().waitFor();
  eq(await page.getByRole('button', { name: 'Collapse flaky', exact: true }).getAttribute('aria-expanded'), 'true', 'it expands');
  const end = rows('flaky', 1).locator('[data-kind="end"]');
  ok((await end.getAttribute('aria-label')).includes('failed'), 'the failed turn ends in a failure');
  ok((await end.getAttribute('class')).includes('tj-bad'), 'marked as one');
  ok((await end.innerText()).includes('✕'), 'with a cross');
  await open(end);
  await page.locator('.tj-detail .tj-events li', { hasText: 'status: failed' }).first().waitFor();
  await page.getByRole('button', { name: 'Collapse flaky', exact: true }).click();
  await page.locator('.tj-child[data-lane="flaky"] .tj-sum').waitFor();
  eq(await rows('flaky').count(), 0, 'it collapses');
  say('LANES');

  where = 'compare';
  // ---- compare ----
  await page.getByRole('button', { name: 'Compare 2 children', exact: true }).click();
  await page.locator('.tj-cmp tbody tr').nth(1).waitFor();
  const table = await page.locator('.tj-cmp tbody tr').evaluateAll((trs) => trs.map((tr) => [tr.dataset.attempt, tr.innerText.replace(/\s+/g, ' ')]));
  eq(table.map((r) => r[0]), ['helper', 'flaky'], 'the better-scored attempt first');
  ok(table[0][1].includes('88') && table[0][1].includes('ready'), `helper's row: ${table[0][1]}`);
  ok(table[1][1].includes('not scored') && table[1][1].includes('failed'), `flaky's row: ${table[1][1]}`);
  ok(await page.locator('.tj-cmp-bar[role="img"][aria-label]').count(), 'each attempt has a text alternative for its bar');
  await page.getByRole('button', { name: 'Hide comparison', exact: true }).click();
  await page.locator('.tj-compare[hidden]').waitFor({ state: 'attached' });
  say('COMPARE');

  where = 'permission';
  // ---- a denied permission, on its own branch ----
  await go('#/trajectory/app/asker');
  await page.getByRole('heading', { level: 1, name: 'Trajectory: asker' }).waitFor();
  await page.locator('.tj-host[data-ready="true"]').waitFor();
  const reads = count(/event-page/);
  const wait1 = page.locator('.tj-turn[data-turn="1"] [data-kind="wait"]');
  await wait1.waitFor();
  eq(await wait1.count(), 1, 'a wait in the turn');
  ok((await wait1.getAttribute('aria-label')).includes('denied'), 'the wait says it was denied');
  ok((await wait1.getAttribute('class')).includes('tj-bad'), 'a denied wait is marked');
  ok((await wait1.innerText()).includes('✕'), 'with a cross');
  eq(count(/event-page/), reads, 'drawing it read no events');
  await open(wait1);
  await page.locator('.tj-detail .tj-events li', { hasText: 'asks to use' }).waitFor();
  const waitDetail = await page.locator('.tj-detail').innerText();
  ok(/Denied/.test(waitDetail), `the wait's answer is shown: ${waitDetail.slice(0, 200)}`);
  eq(count(/event-page/), reads + 1, 'opening it read one page');
  say('PERMISSION');
  await go('#/trajectory/app/lead');
  await page.locator('.tj-host[data-ready="true"]').waitFor();
  await page.locator('.tj-turn[data-lane="lead"][data-turn="3"]').waitFor();

  where = 'synthetic';
  // ---- synthetic timelines: exact geometry ----
  const lab = await context.newPage();
  await lab.goto(`${url}/app/`);
  await lab.locator('#main h1').first().waitFor({ timeout: 60000 });
  const T0 = 1_790_000_000_000;
  const synthetic = await lab.evaluate(async (T0) => {
    const turn = (extra) => ({ seq: 1, started_at_ms: T0, ended_at_ms: T0 + 30000, outcome: { state: 'ready' }, ...extra });
    const lead = {
      turns: [
        turn({
          startup: { provider: 'local', path: 'cold', phases: { queued: 0, placement: 10, sandbox_ready: 50, handshake_done: 80, first_event: 120 } },
          tools: [
            { at_ms: T0 + 2000, name: 'Read', call_id: 'a', seq: 2 },
            { at_ms: T0 + 4000, name: 'Edit', call_id: 'b', seq: 3 },
            { at_ms: T0 + 8000, name: 'Bash', call_id: 'c', seq: 4 },
          ],
          marks: [
            { at_ms: T0 + 9000, seq: 5, kind: 'permission', label: 'write' }, { at_ms: T0 + 12000, seq: 6, kind: 'decision', label: 'write', ok: false },
            { at_ms: T0 + 14000, seq: 7, kind: 'steer' }, { at_ms: T0 + 16000, seq: 8, kind: 'stall' }, { at_ms: T0 + 18000, seq: 9, kind: 'resumed' },
          ],
        }),
        // Two hours later: the gap folds.
        { seq: 20, started_at_ms: T0 + 7_200_000, outcome: null, tools: [], marks: [{ at_ms: T0 + 7_201_000, seq: 21, kind: 'permission', label: 'rm' }] },
      ],
    };
    const many = { turns: [turn({ ended_at_ms: T0 + 5000, tools: Array.from({ length: 500 }, (_, i) => ({ at_ms: T0 + 1000 + Math.floor(i / 20), name: `T${i}`, call_id: `m${i}`, seq: 10 + i })) })] };
    const branches = new Map([
      ['syn', { name: 'syn', harness: 'h', children: [], status: { state: 'running' } }],
      ['many', { name: 'many', harness: 'h', children: [], status: { state: 'ready' } }],
      ['job-a', { name: 'job-a', harness: 'a', profile: 'a', prompt: 'p', depth: 0, created_at: 1, children: [], status: { state: 'ready' } }],
    ]);
    const mountOne = (name, timeline, now) => {
      const host = document.createElement('div');
      host.id = `host-${name}`;
      const wrap = document.createElement('div');
      wrap.className = 'page full tj';
      wrap.append(host);
      // Outside <main>, which the page redraws as its stream moves.
      document.body.append(wrap);
      const handle = window.BranchyardTrajectory.mount(host, {
        repo: 'app', branch: name, now: () => now,
        api: async (path) => { if (/timeline$/.test(path)) return timeline; return { events: [], next_cursor: 0, total: 0 }; },
        path: (b) => `/v1/repos/app/branches/${b}`, branches: () => branches,
        href: { branch: (n) => `#/b/app/${n}`, trajectory: (n) => `#/trajectory/app/${n}` },
        status: (i) => ({ key: i.status.state, text: i.status.state }),
      });
      window[`__${name}`] = handle;
      return host;
    };
    mountOne('syn', lead, T0 + 7_300_000);
    mountOne('many', many, T0 + 9000);
    return true;
  }, T0);
  ok(synthetic, 'the component mounted');
  await lab.locator('#host-syn[data-ready="true"]').waitFor();
  await lab.locator('#host-many[data-ready="true"]').waitFor();
  // Bars in proportion: a 2 s call, a 4 s call, and one cut short by a wait.
  const widths = await lab.locator('#host-syn .tj-turn[data-turn="1"] [data-kind="tool"]').evaluateAll((els) => els.map((e) => [e.getAttribute('aria-label').split(',')[0], e.getBoundingClientRect().width]));
  eq(widths.map((w) => w[0]), ['Tool call Read', 'Tool call Edit', 'Tool call Bash'], 'three calls, drawn');
  const near = (a, b, tol, what) => ok(Math.abs(a - b) <= tol, `${what}: ${a} vs ${b}`);
  near(widths[1][1] / widths[0][1], 2, 0.1, 'a 4 s call is twice a 2 s call');
  // A wait from 9 s to 12 s, denied; the call it paused continues after it.
  const waitBox = await lab.locator('#host-syn [data-kind="wait"]').first().evaluate((e) => [e.getBoundingClientRect().width, e.getAttribute('aria-label'), e.className]);
  ok(waitBox[1].includes('denied') && waitBox[2].includes('tj-bad'), `a denied wait: ${waitBox}`);
  near(waitBox[0] / widths[0][1], 1.5, 0.1, 'a 3 s wait is 1.5 times a 2 s call');
  eq(await lab.locator('#host-syn [data-kind="stall"]').count(), 1, 'a stall');
  eq(await lab.locator('#host-syn [data-kind="steer"]').count(), 1, 'a steer');
  eq(await lab.locator('#host-syn .tj-turn[data-turn="1"] .tj-phase').count(), 5, 'a slice for each startup phase');
  // The second turn still runs: an open wait, a live marker, an idle gap folded.
  const openWait = lab.locator('#host-syn .tj-turn[data-turn="2"] [data-kind="wait"]');
  ok((await openWait.getAttribute('class')).includes('tj-open'), 'an unanswered wait is open');
  ok((await openWait.getAttribute('aria-label')).includes('still open'), 'and says so');
  eq(await lab.locator('#host-syn .tj-turn[data-turn="2"] .tj-head').count(), 1, 'a running turn has its live head');
  ok(await lab.locator('#host-syn .tj-axis .tj-break').count(), 'two hours idle are folded');
  const folded = await lab.evaluate(() => document.querySelector('#host-syn .tj-track').getBoundingClientRect().width);
  await lab.locator('#host-syn .tj-check input').uncheck();
  await lab.waitForFunction(() => !document.querySelector('#host-syn .tj-axis .tj-break'));
  const real = await lab.evaluate(() => document.querySelector('#host-syn .tj-track').getBoundingClientRect().width);
  eq(await lab.locator('#host-syn .tj-axis .tj-break').count(), 0, 'unfolded, no break');
  ok(real <= folded * 1.2, `the track keeps the fitted width: ${real} vs ${folded}`);
  const turn2x = await lab.evaluate(() => { const t = document.querySelector('#host-syn .tj-turn[data-turn="2"] [data-kind="prompt"]'); return t.getBoundingClientRect().left - document.querySelector('#host-syn .tj-track').getBoundingClientRect().left; });
  ok(turn2x > folded * 0.9, `unfolded, the second turn is at the far right: ${turn2x} of ${folded}`);
  await lab.locator('#host-syn .tj-check input').check();
  await lab.waitForFunction(() => document.querySelector('#host-syn .tj-axis .tj-break'));
  // Zoom grows the track; the page does not widen.
  await lab.getByRole('button', { name: 'Zoom in' }).first().click();
  await lab.waitForFunction(() => document.querySelector('#host-syn .tj-zoomval').textContent === '160%');
  const zoomed = await lab.evaluate(() => document.querySelector('#host-syn .tj-track').getBoundingClientRect().width);
  near(zoomed / folded, 1.6, 0.1, 'zoom 160% widens the track');
  const widthNow = await lab.evaluate(() => [document.documentElement.scrollWidth, innerWidth]);
  ok(widthNow[0] <= widthNow[1], `zooming does not widen the page: ${widthNow}`);
  // 500 calls in 5 s draw as a few clusters, not 500 buttons; a cluster names its count.
  const clusters = await lab.locator('#host-many [data-kind="tool"]').count();
  ok(clusters < 60, `500 calls are clustered: ${clusters} buttons`);
  const clusterLabel = await lab.locator('#host-many .tj-cluster').first().getAttribute('aria-label');
  ok(/tool calls from .* zoom in/.test(clusterLabel), `a cluster says how many and how to separate them: ${clusterLabel}`);
  // The stream: a call on a running turn extends it, a status ends it, with no read.
  await lab.evaluate((T0) => {
    window.__syn.activity('syn', { decision: { tool: 'rm', allowed: true, message: null, source: 'asked' } }, T0 + 7_250_000);
    window.__syn.activity('syn', { harness: { type: 'tool_started', turn: 2, call_id: 'z', name: 'Glob' } }, T0 + 7_260_000);
  }, T0);
  await lab.locator('#host-syn .tj-turn[data-turn="2"] [data-kind="tool"]').waitFor();
  const answered = await lab.locator('#host-syn .tj-turn[data-turn="2"] [data-kind="wait"]').getAttribute('aria-label');
  ok(answered.includes('allowed') && !answered.includes('still open'), `the stream answered the wait: ${answered}`);
  await lab.close();
  say('SYNTHETIC');

  where = 'live';
  // ---- live ----
  const eventReads = count(/event-page/);
  const leadReads = count(/\/branches\/lead\/timeline$/);
  say('LIVE READY');
  await nextLine();
  await page.locator('.tj-turn[data-lane="lead"][data-turn="4"]').waitFor({ timeout: 60000 });
  const liveCalls = await rows('lead', 4).locator('[data-kind="tool"]').evaluateAll((els) => els.map((e) => e.getAttribute('aria-label')));
  ok(liveCalls.length >= 1, 'the new turn drew its calls');
  eq(await page.locator('[data-stat="turns"]').innerText(), '4', 'the summary counts the new turn');
  await rows('lead', 4).locator('[data-kind="end"]').waitFor({ timeout: 60000 });
  eq(count(/\/branches\/lead\/timeline$/), leadReads, 'the stream was applied without reading the timeline again');
  eq(count(/event-page/), eventReads, 'and without reading events nobody opened');
  say('LIVE');

  where = 'viewports';
  // ---- viewports ----
  await noOverflow('the trajectory page');
  const wide = await page.evaluate(() => { const s = document.querySelector('.tj-scroll'); return [s.scrollWidth, s.clientWidth]; });
  ok(wide[0] >= wide[1], 'the waterfall keeps its own width');
  if (shots) await page.screenshot({ path: `${shots}/trajectory-desktop.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'dark' });
  eq(await page.evaluate(() => getComputedStyle(document.body).backgroundColor !== 'rgb(244, 243, 239)'), true, 'dark follows the device');
  await noOverflow('the dark trajectory page');
  if (shots) await page.screenshot({ path: `${shots}/trajectory-desktop-dark.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'light' });
  await page.setViewportSize({ width: 390, height: 844 });
  await page.locator('.tj-host[data-ready="true"]').waitFor();
  await noOverflow('the trajectory page on a phone');
  const phone = await page.evaluate(() => { const s = document.querySelector('.tj-scroll'); return [s.scrollWidth, s.clientWidth, s.getBoundingClientRect().right, innerWidth]; });
  ok(phone[2] <= phone[3], `the waterfall fits the phone: ${phone}`);
  ok(phone[0] > phone[1], `and scrolls inside itself: ${phone}`);
  await open(rows('lead', 1).locator('[data-kind="prompt"]'));
  await page.locator('.tj-detail .tj-events li').first().waitFor();
  await noOverflow('the detail on a phone');
  await page.getByRole('button', { name: 'Expand flaky', exact: true }).click();
  await rows('flaky').first().waitFor();
  await noOverflow('an expanded lane on a phone');
  if (shots) await page.screenshot({ path: `${shots}/trajectory-phone.png`, fullPage: true });
  await page.emulateMedia({ colorScheme: 'dark' });
  if (shots) await page.screenshot({ path: `${shots}/trajectory-phone-dark.png`, fullPage: true });
  eq(problems, [], 'the page logged no error');
  say('VIEWPORTS');
  say('DONE');
  await browser.close();
  // The controller keeps stdin open until this process ends.
  process.exit(0);
})().catch(async (e) => {
  console.error(e && e.stack ? e.stack : e);
  process.exit(1);
});
