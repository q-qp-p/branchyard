// The Branchyard companion: one page over the server's own HTTP API and
// event stream (docs/companion.md). Every action is an API call made with
// the bearer token this tab holds, under that token's scopes; the page
// adds no authority of its own. Text from the server is only ever set as
// text (textContent), never parsed as HTML.
'use strict';

(() => {
  const TOKEN_KEY = 'branchyard.token';
  const REPO_KEY = 'branchyard.repo';
  const SOURCE_KEY = 'branchyard.source';
  // What `R` in `by watch` sends to an interrupted branch.
  const RESUME_PROMPT = 'Your previous turn was interrupted before it finished. ' +
    'Continue the task from where you left off, then summarize what you did.';
  // What "Collect outputs" asks of a new session: a required deliverable
  // whose declared files the server publishes as the workspace's outputs
  // after the turn (docs/artifact-capture.md).
  const OUTPUT_MANIFEST = 'outputs/artifacts.json';
  const OUTPUT_CONTRACT = {
    schema: { type: 'object', required: ['summary'], properties: { summary: { type: 'string' } } },
    required: true, purpose: 'deliverable', capture: { manifest: OUTPUT_MANIFEST },
  };
  const DIFF_LIMIT = 400000;
  // The server's largest event page; an open branch keeps at most
  // EVENT_KEEP of its latest events.
  const EVENT_WINDOW = 200;
  const EVENT_KEEP = 2000;
  const SVG = 'http://www.w3.org/2000/svg';

  const state = {
    token: null,
    me: null,
    repos: [],
    repo: null,
    // What the sidebar lists: a repository's branches ({ kind: 'repo' },
    // the one in `repo`), a project's workspaces ({ kind: 'project', id }),
    // or nothing yet ({ kind: 'none' }).
    source: { kind: 'none' },
    // null when this server has no projects; otherwise every project.
    projects: null,
    // The listed project's workspaces.
    workspaces: { project: null, list: [], loaded: false, error: null },
    branches: new Map(),
    lastActive: new Map(),
    cursor: null,
    stream: null,
    backoff: 1000,
    loaded: false,
    branchError: null,
    filter: '',
    doneOpen: false,
    boardDoneOpen: false,
    seen: new Set(),
    statusSeen: new Map(),
    permissions: [],
    inbox: [],
    approvals: [],
    effects: [],
    // What waits for a person, as the server orders it (`GET …/decisions`).
    decisions: [],
    // Which files are in whose hands (`GET …/ground`), with the repository
    // it was read for; null until read.
    ground: null,
    // The same for the listed project's workspaces (`GET /v1/projects/{id}/decisions`):
    // outputs waiting to be accepted and sessions that stopped; and the
    // sessions its board shows running.
    projectDecisions: { project: null, list: [] },
    projectSessions: { project: null, list: [] },
    // The deck: cards sent to the back, the one brought to the front, the
    // answers held for their undo window, and the evidence fetched for a
    // card (a branch's last turn and its diff).
    deck: { later: [], front: null, holding: [], evidence: new Map() },
    pending: new Map(),
    merged: new Set(),
    route: { name: 'home' },
    // The branch whose board row has its steer box open.
    steering: null,
    // Each branch's turns on the board's clock, read from its latest
    // events: { at, turns: [{ start, end, tools: [ms] }], merged }.
    tracks: new Map(),
    // Approvals already answered, newest first: the deck's record of the day.
    answered: [],
    pushInfo: null,
    pushOn: false,
  };

  // ---------------------------------------------------------------
  // Storage: the token lives in this tab's sessionStorage only.

  function load(key) {
    try { return sessionStorage.getItem(key); } catch (_) { return null; }
  }
  function save(key, value) {
    try {
      if (value == null) sessionStorage.removeItem(key); else sessionStorage.setItem(key, value);
    } catch (_) { /* private mode: kept in memory for this page only */ }
  }

  // ---------------------------------------------------------------
  // Elements

  function h(tag, props, ...children) {
    const el = document.createElement(tag);
    for (const [key, value] of Object.entries(props || {})) {
      if (value == null || value === false) continue;
      if (key === 'class') el.className = value;
      else if (key === 'text') el.textContent = value;
      else if (key.startsWith('on')) el.addEventListener(key.slice(2), value);
      else if (value === true) el.setAttribute(key, '');
      else el.setAttribute(key, String(value));
    }
    for (const child of children.flat(Infinity)) {
      if (child == null || child === false) continue;
      el.append(child instanceof Node ? child : document.createTextNode(String(child)));
    }
    return el;
  }
  const $ = (id) => document.getElementById(id);
  const enc = encodeURIComponent;
  const cap = (text) => text.charAt(0).toUpperCase() + text.slice(1);
  // A `when` reason as a sentence; one that opens with a branch's name keeps its case.
  const sentence = (why, name) => `${name && why.startsWith(name) ? why : cap(why)}.`;
  const plural = (n, one, many = `${one}s`) => `${n} ${n === 1 ? one : many}`;
  const branchHref = (name) => `#/b/${enc(state.repo)}/${enc(name)}`;
  let uid = 0;
  const nextId = (prefix) => `${prefix}-${++uid}`;

  // Replace an element's children, skipping what is null or false, as h() does.
  function fill(el, ...children) {
    el.replaceChildren(...children.flat(Infinity).filter((c) => c != null && c !== false));
    return el;
  }

  // A page's heading takes focus on navigation, and keeps it when a live
  // update redraws the page.
  function main(...children) {
    const m = $('main');
    const kept = document.activeElement && document.activeElement.tagName === 'H1' && m.contains(document.activeElement);
    m.removeAttribute('aria-busy');
    fill(m, ...children);
    const heading = m.querySelector('h1');
    if (heading) heading.tabIndex = -1;
    if (heading && kept) heading.focus({ preventScroll: true });
  }

  function focusHeading() {
    const heading = $('main').querySelector('h1');
    if (heading) heading.focus({ preventScroll: false });
  }

  const dot = (lane) => h('span', { class: `dot dot-${lane}`, 'aria-hidden': 'true' });

  function counts(insertions, deletions) {
    return [insertions || !deletions ? h('span', { class: 'add', text: `+${insertions}` }) : null,
      insertions && deletions ? ' ' : null,
      deletions ? h('span', { class: 'del', text: `−${deletions}` }) : null];
  }

  function checkMark(drawn) {
    const svg = document.createElementNS(SVG, 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('class', drawn ? 'check drawn' : 'check');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', 'M3 8.5l3.2 3L13 4.5');
    svg.append(path);
    return svg;
  }

  // Inline icons: strokes on a 16-unit grid, drawn here so the page loads
  // no file for them. Each is decorative; the text beside it says what it is.
  const ICONS = {
    plus: 'M8 3.5v9M3.5 8h9',
    dots: 'M3.5 8h.01M8 8h.01M12.5 8h.01',
    copy: 'M6 6h7v7H6zM3 10V3h7',
    search: 'M7 12a5 5 0 1 1 0-10 5 5 0 0 1 0 10zM10.6 10.6L14 14',
    terminal: 'M3 4l4 4-4 4M8.5 12H13',
    eye: 'M1.5 8s2.5-4.5 6.5-4.5S14.5 8 14.5 8 12 12.5 8 12.5 1.5 8 1.5 8zM8 9.8a1.8 1.8 0 1 0 0-3.6 1.8 1.8 0 0 0 0 3.6z',
    pencil: 'M11.5 2.5l2 2L5 13H3v-2zM9.5 4.5l2 2',
    globe: 'M8 14.5a6.5 6.5 0 1 0 0-13 6.5 6.5 0 0 0 0 13zM1.5 8h13M8 1.5c2 2 2 11 0 13M8 1.5c-2 2-2 11 0 13',
    bolt: 'M9 1.5L3 9h4l-1 5.5L13 7H9z',
    check: 'M3 8.5l3.2 3L13 4.5',
    cross: 'M4 4l8 8M12 4l-8 8',
    merge: 'M2.5 8h7.5M7 5l3 3-3 3M13 2.5v11',
    wrench: 'M9.6 2.4a3.3 3.3 0 0 0 4 4.2l-5.8 5.8a1.5 1.5 0 0 1-2.2-2.2l5.8-5.8a3.3 3.3 0 0 0-1.8-2z',
  };
  function icon(name, small) {
    const svg = document.createElementNS(SVG, 'svg');
    svg.setAttribute('viewBox', '0 0 16 16');
    svg.setAttribute('class', small ? 'icon small' : 'icon');
    svg.setAttribute('aria-hidden', 'true');
    const path = document.createElementNS(SVG, 'path');
    path.setAttribute('d', ICONS[name]);
    svg.append(path);
    return svg;
  }
  // A tool call's icon, from the verb its name opens with.
  const TOOL_ICONS = [
    [/^(run|bash|sh|shell|exec|execute|cargo|npm|make|python|node|git)\b/i, 'terminal'],
    [/^(read|view|cat|open|inspect|show)\b/i, 'eye'],
    [/^(edit|write|create|patch|replace|apply|update|delete|move|rename)\b/i, 'pencil'],
    [/^(search|grep|glob|find|list|ls)\b/i, 'search'],
    [/^(fetch|web|http|browse|curl|download)\b/i, 'globe'],
    [/^(mcp|tool|call|task|agent|delegate)\b/i, 'bolt'],
  ];
  const toolIcon = (name) => (TOOL_ICONS.find(([re]) => re.test(name)) || [null, 'wrench'])[1];
  // What the page shows where a conversation will be: a few quiet bars.
  const skeleton = () => h('div', { class: 'skeleton', role: 'status', 'aria-label': 'Loading the conversation' }, h('i'), h('i'), h('i'), h('i'));

  // Only what the person did here gets a toast: the branch list is the
  // notice for everything else.
  function toast(text, href) {
    const box = h('div', { class: 'toast' }, text, href ? [' ', h('a', { href, text: 'Open' })] : null);
    const toasts = $('toasts');
    toasts.append(box);
    while (toasts.children.length > 2) toasts.firstChild.remove();
    setTimeout(() => box.remove(), 5000);
  }

  function setLive(which, text) {
    const live = $('live');
    live.dataset.state = which;
    live.textContent = text || which;
  }

  function confirmDialog(title, text, okText, danger) {
    const dialog = $('confirm');
    $('confirm-title').textContent = title;
    $('confirm-text').textContent = text;
    const ok = $('confirm-ok');
    ok.textContent = okText || 'Confirm';
    ok.className = danger ? 'danger' : 'primary';
    if (typeof dialog.showModal !== 'function') return Promise.resolve(window.confirm(`${title}\n\n${text}`));
    return new Promise((resolve) => {
      const done = (answer) => {
        ok.removeEventListener('click', yes);
        $('confirm-cancel').removeEventListener('click', no);
        dialog.removeEventListener('cancel', no);
        if (dialog.open) dialog.close();
        resolve(answer);
      };
      const yes = () => done(true);
      const no = () => done(false);
      ok.addEventListener('click', yes);
      $('confirm-cancel').addEventListener('click', no);
      dialog.addEventListener('cancel', no);
      dialog.showModal();
      $('confirm-cancel').focus();
    });
  }

  // ---------------------------------------------------------------
  // The API

  class ApiError extends Error {
    constructor(status, code, message, detail) {
      super(message);
      this.status = status;
      this.code = code;
      this.detail = detail || null;
    }
  }

  function key() {
    if (window.crypto && crypto.randomUUID) return crypto.randomUUID();
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    return Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('');
  }

  // Every request carries this tab's token, and nothing else of the
  // browser's (no cookies, no cache).
  function authFetch(path, init = {}) {
    const headers = new Headers(init.headers || {});
    if (state.token) headers.set('Authorization', `Bearer ${state.token}`);
    return fetch(path, { ...init, headers, cache: 'no-store', credentials: 'omit' });
  }

  // A response as JSON, or its error envelope as an ApiError.
  async function call(path, init) {
    let response;
    try {
      response = await authFetch(path, init);
    } catch (e) {
      throw new ApiError(0, 'unavailable', 'The server cannot be reached.');
    }
    const text = await response.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch (_) { json = null; }
    if (!response.ok) {
      const err = (json && json.error) || {};
      if (response.status === 401 && state.me) {
        signOut('This token is no longer accepted (expired or revoked). Pair again.');
      }
      throw new ApiError(response.status, err.code || 'error', err.message || `HTTP ${response.status}`, err.detail);
    }
    return json;
  }

  function api(method, path, body) {
    const headers = { Accept: 'application/json' };
    const init = { method, headers };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
      if (method === 'POST') headers['Idempotency-Key'] = key();
    }
    return call(path, init);
  }

  // An operation polled until it settles, at `by`'s cadence: every second
  // at first, then every three. null when it is still going after half an
  // hour, or this tab signed out.
  async function awaitOperation(id) {
    for (let i = 0; i < 600 && state.token; i++) {
      await new Promise((r) => setTimeout(r, i < 10 ? 1000 : 3000));
      let op;
      try { op = await api('GET', `/v1/operations/${enc(id)}`); } catch (_) { continue; }
      if (['succeeded', 'failed', 'interrupted'].includes(op.state)) return op;
    }
    return null;
  }

  // Save a response's body under its name, through a Blob URL that is
  // revoked once the download has had time to start.
  async function download(path, name) {
    let blob;
    try {
      const response = await authFetch(path);
      if (!response.ok) {
        let message = `HTTP ${response.status}`;
        try { message = (await response.json()).error.message || message; } catch (_) { /* not JSON */ }
        throw new Error(message);
      }
      blob = await response.blob();
    } catch (e) {
      toast(`Could not download ${name}: ${e instanceof TypeError ? 'the server cannot be reached' : e.message}.`);
      return;
    }
    const url = URL.createObjectURL(blob);
    const link = h('a', { href: url, download: name, hidden: true });
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 60000);
  }

  const can = (scope) => !!(state.me && state.me.scopes.includes(scope));
  const repoPath = (repo) => `/v1/repos/${enc(repo || state.repo)}`;
  const branchPath = (branch, repo) => `${repoPath(repo)}/branches/${enc(branch)}`;

  // ---------------------------------------------------------------
  // Statuses, the same words `by ls` uses

  function statusOf(info) {
    const s = info.status || {};
    if (info.stalled && s.state === 'running') return { key: 'stalled', text: 'running, stalled' };
    switch (s.state) {
      case 'running': return { key: 'running', text: 'running' };
      case 'waiting': return { key: 'waiting', text: 'waiting' };
      case 'blocked': return { key: 'blocked', text: `blocked: ${s.reason}` };
      case 'ready': return { key: 'ready', text: 'ready' };
      case 'no_changes': return { key: 'no_changes', text: 'no changes' };
      case 'interrupted': return { key: 'interrupted', text: 'interrupted' };
      case 'budget_exceeded': return { key: 'budget_exceeded', text: `over budget (${s.limit})` };
      case 'failed': return { key: 'failed', text: `failed: ${s.reason}` };
      case 'merged': return { key: 'merged', text: `merged into ${s.target}` };
      case 'awaiting_plan_approval': return { key: 'awaiting_plan_approval', text: 'awaiting plan approval' };
      default: return { key: 'unknown', text: s.state || 'unknown' };
    }
  }

  // When each action applies: the rules of `by watch`'s action registry.
  const when = {
    send(info) {
      const st = info.status.state;
      if (st === 'running') return `${info.name} has a turn running; steer it or cancel it`;
      if (st === 'waiting') return `${info.name} is waiting for its prerequisites and has no session yet`;
      return null;
    },
    steer(info) {
      return info.status.state === 'running' ? null : `${info.name} has no turn running; send a new prompt instead`;
    },
    resume(info) {
      return info.status.state === 'interrupted' ? null : 'only an interrupted branch resumes';
    },
    cancel(info) {
      return ['running', 'waiting'].includes(info.status.state) ? null : `nothing to cancel: ${info.name} is ${statusOf(info).text}`;
    },
    merge(info) {
      return info.status.state === 'ready' && info.candidate ? null : `only a ready branch merges; ${info.name} is ${statusOf(info).text}`;
    },
    fork(info) {
      return ['waiting', 'blocked'].includes(info.status.state) ? `${info.name} has not started, so there is nothing to fork` : null;
    },
    diff(info) {
      return info.candidate ? null : `${info.name} has no candidate commit yet`;
    },
  };

  // Branches grouped by what they ask of you, in display order: the first
  // lane whose test passes takes the branch. `attention` maps a branch to
  // the approval or question it waits on a person for.
  const LANES = [
    {
      key: 'needs', label: 'Needs you', collapsed: false,
      test: (b, attention) => attention.has(b.name) || b.stalled ||
        ['blocked', 'failed', 'interrupted', 'budget_exceeded', 'awaiting_plan_approval'].includes(b.status.state),
      meta: (b, attention) => (b.awaiting ? 'waiting for you' : attention.get(b.name) || (b.stalled ? 'stalled' : statusOf(b).text.split(/[:(]/)[0].trim())),
    },
    {
      key: 'running', label: 'Running', collapsed: false,
      test: (b) => ['running', 'waiting'].includes(b.status.state),
      meta: (b) => b.status.state,
    },
    {
      key: 'ready', label: 'Ready', collapsed: false,
      test: (b) => b.status.state === 'ready',
      meta: (b) => (b.candidate ? counts(b.candidate.insertions, b.candidate.deletions) : 'ready'),
    },
    {
      key: 'done', label: 'Done', collapsed: true,
      test: () => true,
      meta: (b) => (b.status.state === 'merged' ? 'merged' : statusOf(b).text),
    },
  ];

  function attention() {
    const waiting = new Map();
    for (const m of state.inbox) waiting.set(m.from, m.kind);
    for (const a of state.approvals) waiting.set(a.branch, 'approval');
    for (const b of state.branches.values()) if (b.awaiting) waiting.set(b.name, 'approval');
    return waiting;
  }

  const laneOf = (b, waiting) => LANES.find((lane) => lane.test(b, waiting));
  const activeAt = (b) => state.lastActive.get(b.name) || b.created_at * 1000;

  function lanes(branches) {
    const waiting = attention();
    const sorted = branches.slice().sort((x, y) => activeAt(y) - activeAt(x));
    return LANES.map((lane) => ({ lane, branches: sorted.filter((b) => laneOf(b, waiting) === lane) }));
  }

  // Workspaces grouped by state in the same order and colours: one that
  // failed to start needs you; one starting, running, archiving or
  // restoring runs; an idle one is ready for work; archived ones fold away.
  const WORKSPACE_LANES = [
    { key: 'needs', label: 'Needs you', collapsed: false, states: ['failed', 'interrupted'] },
    { key: 'running', label: 'Running', collapsed: false, states: ['running', 'pending', 'restoring', 'archiving'] },
    { key: 'ready', label: 'Ready', collapsed: false, states: ['ready'] },
    { key: 'done', label: 'Archived', collapsed: true, states: null },
  ];
  const workspaceLane = (w) => WORKSPACE_LANES.find((lane) => !lane.states || lane.states.includes(w.state));
  const workspaceStateText = (w) => ({ pending: 'starting' }[w.state] || w.state);

  function money(v) {
    return v == null ? '—' : `$${Number(v).toFixed(v < 1 ? 4 : 2)}`;
  }
  // Dollars to the cent, for a column of them.
  const cents = (v) => (v == null ? '—' : v > 0 && v < 0.005 ? '<$0.01' : `$${Number(v).toFixed(2)}`);

  function since(ms) {
    const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
    if (s < 60) return `${s}s`;
    if (s < 3600) return `${Math.round(s / 60)}m`;
    if (s < 86400) return `${Math.round(s / 3600)}h`;
    return `${Math.round(s / 86400)}d`;
  }

  // A relative time that ticks (see boot) without redrawing its page.
  function ago(ms, suffix = ' ago') {
    return h('time', { datetime: new Date(ms).toISOString(), 'data-at': ms, 'data-suffix': suffix, text: since(ms) + suffix });
  }

  function clock(ms) {
    const d = new Date(ms);
    return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  }

  // ---------------------------------------------------------------
  // What deserves a system notification: `by watch`'s rules, each once

  function notice(entry) {
    const a = entry.activity || {};
    const b = entry.branch;
    let kind = null;
    let text = null;
    let target = b;
    if (a.harness && a.harness.type === 'permission_requested') {
      const k = `perm:${b}:${JSON.stringify(a.harness.request.key)}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'permission';
      text = `${b} asks to use ${a.harness.request.tool}`;
    } else if (a.message && ['question', 'escalation'].includes(a.message.kind)) {
      const k = `msg:${a.message.id}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'question';
      target = a.message.to;
      text = `${a.message.from} ${a.message.kind === 'question' ? 'asks' : 'escalates to'} ${a.message.to}: ${a.message.text}`;
    } else if (a.effect && a.effect.kind === 'asked') {
      const k = `approval:${a.effect.ask}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'approval';
      text = `${b} waits for approval: ${aboutText(a.effect.about)}`;
    } else if (a.stalled) {
      const k = `stall:${b}:${a.stalled.since_ms}`;
      if (state.seen.has(k)) return null;
      state.seen.add(k);
      kind = 'stalled';
      text = `${b} has stalled`;
    } else if (a.status) {
      const st = a.status.state;
      if (st === 'running' || st === 'waiting') { state.statusSeen.delete(b); return null; }
      if (st === 'merged') return null;
      const map = {
        ready: ['finished', `${b} is ready to merge`],
        no_changes: ['finished', `${b} finished with no changes`],
        budget_exceeded: ['finished', `${b} stopped at its budget`],
        interrupted: ['interrupted', `${b} was interrupted`],
        failed: ['failed', `${b} failed: ${a.status.reason}`],
        blocked: ['failed', `${b} is blocked: ${a.status.reason}`],
        awaiting_plan_approval: ['question', `${b} has a plan waiting for your approval`],
      };
      if (!map[st]) return null;
      [kind, text] = map[st];
      if (state.statusSeen.get(b) === kind) return null;
      state.statusSeen.set(b, kind);
    } else {
      return null;
    }
    const href = kind === 'question' || kind === 'approval' ? '#/attention' : branchHref(target);
    return { kind, text, href };
  }

  // ---------------------------------------------------------------
  // Signing in and pairing

  function signOut(message) {
    state.token = null;
    state.me = null;
    save(TOKEN_KEY, null);
    if (state.stream) state.stream.abort();
    state.stream = null;
    closePreview();
    closeWorkspace();
    clearTimeout(workspacesTimer);
    Object.assign(state, {
      repos: [], repo: null, source: { kind: 'none' }, projects: null,
      workspaces: { project: null, list: [], loaded: false, error: null },
      branches: new Map(), lastActive: new Map(), loaded: false, branchError: null,
      permissions: [], inbox: [], approvals: [], answered: [], effects: [], decisions: [], tracks: new Map(),
      projectDecisions: { project: null, list: [] }, projectSessions: { project: null, list: [] },
    });
    $('sidebar').hidden = true;
    document.title = 'Branchyard';
    setLive('off', 'signed out');
    renderSignIn(message);
  }

  function renderSignIn(message) {
    document.body.dataset.view = 'signin';
    const id = nextId('token');
    const error = h('p', { class: 'error', role: 'alert', text: message || '' });
    const input = h('input', { id, type: 'password', autocomplete: 'off', spellcheck: 'false', required: true });
    const connect = h('button', { type: 'button', class: 'primary', text: 'Connect to server', onclick: () => submit() });
    const submit = async () => {
      const token = input.value.trim();
      if (!token) { input.focus(); input.reportValidity(); return; }
      if (connect.disabled) return;
      connect.disabled = true;
      connect.textContent = 'Connecting…';
      error.textContent = '';
      state.token = token;
      try {
        await start();
        save(TOKEN_KEY, token);
      } catch (e) {
        state.token = null;
        error.textContent = e.status === 401 ? 'That token is not accepted.' : e.message;
        connect.disabled = false;
        connect.textContent = 'Connect to server';
      }
    };
    input.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') submit(); });
    main(h('div', { class: 'signin' },
      h('img', { class: 'signin-mark', src: 'icon.svg', alt: '', width: 32, height: 32 }),
      h('h1', { text: 'Connect to Branchyard' }),
      h('p', { class: 'lede', text: 'Follow your tasks and review their branches from this browser.' }),
      h('section', { class: 'card connect', 'aria-label': 'Connect to your server' },
        h('h2', { text: 'Open a pairing link' }),
        h('p', {}, 'On the machine running your server, run ', h('code', { text: 'by serve token new --link' }),
          '. Open the printed link on this device.'),
        h('h2', { text: 'Or use a server token' }),
        h('label', { class: 'block', for: id, text: 'Server token' }), input,
        h('p', { class: 'hint', text: 'Kept only in this browser tab and sent only to this server.' }),
        h('div', { class: 'row form-actions' }, connect)),
      error));
    focusHeading();
  }

  function deviceLabel() {
    const ua = navigator.userAgent || '';
    const browser = /Firefox\//.test(ua) ? 'Firefox' : /Edg\//.test(ua) ? 'Edge'
      : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'Browser';
    const os = /Android/.test(ua) ? 'Android' : /iPhone|iPad/.test(ua) ? 'iOS' : /Mac OS X/.test(ua) ? 'macOS'
      : /Windows/.test(ua) ? 'Windows' : /Linux/.test(ua) ? 'Linux' : '';
    return os ? `${browser} on ${os}` : browser;
  }

  async function pair(code) {
    // Take the code out of the address bar and history first.
    history.replaceState(null, '', location.pathname + location.search);
    document.body.dataset.view = 'signin';
    main(h('div', { class: 'signin' }, h('h1', { text: 'Pairing…' })));
    try {
      const paired = await api('POST', '/app/pair', { code, device: deviceLabel() });
      state.token = paired.token;
      save(TOKEN_KEY, paired.token);
      toast(`Paired as ${paired.me.name}.`);
      return true;
    } catch (e) {
      renderSignIn(e.code === 'rate_limited' ? e.message : `Pairing failed: ${e.message}`);
      return false;
    }
  }

  // ---------------------------------------------------------------
  // Loading and the live stream

  // A token that reaches no repository still has a console when this
  // server has projects: knowledge work needs none.
  async function start() {
    state.me = await api('GET', '/v1/app/me');
    const [{ repos }, projects] = await Promise.all([api('GET', '/v1/repos'), loadProjects()]);
    state.repos = repos.map((r) => r.name);
    state.projects = projects instanceof ApiError ? null : projects;
    if (!state.repos.length && !state.projects) {
      if (projects instanceof ApiError) throw projects;
      throw new ApiError(403, 'no_repos', 'This token can reach no repository.');
    }
    if (projects instanceof ApiError) toast(`Projects did not load: ${projects.message}`);
    const asked = parseRoute();
    const remembered = load(REPO_KEY);
    state.repo = state.repos.includes(asked.repo) ? asked.repo : state.repos.includes(remembered) ? remembered : state.repos[0] || null;
    state.source = initialSource(asked);
    renderPicker();
    $('sidebar').hidden = false;
    $('live').hidden = !state.repo;
    const loading = state.repo ? switchRepo(state.repo) : Promise.resolve();
    if (state.source.kind === 'project') loadWorkspaces(state.source.id);
    route();
    await loading;
  }

  // ---------------------------------------------------------------
  // Sources: repositories and projects, in the sidebar's picker

  // Every project; null on a server without them; the error when the list
  // failed, so a token with repositories still gets its branches.
  async function loadProjects() {
    try {
      return (await api('GET', '/v1/projects')).projects;
    } catch (e) {
      if (e.status === 404) return null;
      if (e.status === 401) throw e;
      return e;
    }
  }

  const projectOf = (id) => (state.projects || []).find((p) => p.id === id) || null;

  // A workspace's route names no project: opening it shows its own.
  function initialSource(asked) {
    if (asked.name === 'branch' && state.repo) return { kind: 'repo' };
    if (asked.name === 'project') return { kind: 'project', id: asked.project };
    const remembered = /^project:(.+)$/.exec(load(SOURCE_KEY) || '');
    if (remembered && projectOf(remembered[1])) return { kind: 'project', id: remembered[1] };
    if (state.repo) return { kind: 'repo' };
    if (state.projects && state.projects.length) return { kind: 'project', id: state.projects[0].id };
    return { kind: 'none' };
  }

  function useSource(source) {
    state.source = source;
    save(SOURCE_KEY, source.kind === 'project' ? `project:${source.id}` : source.kind);
    renderPicker();
  }

  // One select for both: Repositories, then Projects, then "New project…".
  function renderPicker() {
    const select = $('repo');
    const current = state.source.kind === 'project' ? `project:${state.source.id}` : state.repo ? `repo:${state.repo}` : '';
    const groups = [];
    if (state.repos.length) {
      groups.push(h('optgroup', { label: 'Repositories' }, state.repos.map((r) => h('option', { value: `repo:${r}`, text: r }))));
    }
    if (state.projects && state.projects.length) {
      groups.push(h('optgroup', { label: 'Projects' }, state.projects.map((p) => h('option', { value: `project:${p.id}`, text: p.name }))));
    }
    const creating = state.projects && can('run');
    fill(select, current ? null : h('option', { value: '', text: 'Choose a project', disabled: true }), groups,
      creating ? h('option', { value: 'new-project', text: 'New project…' }) : null);
    select.value = current;
    const options = select.options.length;
    $('repo-pick').hidden = options < 2;
    $('repo-name').hidden = options > 1;
    $('repo-name').textContent = current.startsWith('repo:') ? state.repo : (projectOf(state.source.id) || {}).name || '';
    updateSideLinks();
  }

  async function pickSource(value) {
    if (value === 'new-project') { renderPicker(); go('#/p/new'); return; }
    const at = value.indexOf(':');
    const kind = value.slice(0, at);
    const name = value.slice(at + 1);
    if (kind === 'project') { go(`#/p/${enc(name)}`); return; }
    closePreview();
    closeWorkspace();
    useSource({ kind: 'repo' });
    if (name !== state.repo) await switchRepo(name);
    renderPicker();
    go('#/');
  }

  // Navigate, or route again when the address already says so.
  function go(hash) {
    if (location.hash === hash) route(); else location.hash = hash;
  }

  // The sidebar's link to start work follows the source.
  function updateSideLinks() {
    const link = $('new-task');
    const project = state.source.kind === 'project' ? state.source.id : null;
    link.hidden = !can('run') || (!project && !state.repo);
    link.href = project ? `#/p/${enc(project)}/new` : '#/new';
    link.dataset.route = project ? 'newWorkspace' : 'new';
    link.lastChild.textContent = project ? 'New workspace' : 'New task';
    const what = project ? 'workspaces' : 'branches';
    $('branch-filter').placeholder = `Filter ${what}`;
    $('branch-filter-label').textContent = `Filter ${what}`;
    $('sidebar').setAttribute('aria-label', cap(what));
  }

  // The listed project's workspaces, refreshed every few seconds while
  // one of them is starting, running, archiving or restoring.
  let workspacesTimer = null;
  async function loadWorkspaces(project) {
    clearTimeout(workspacesTimer);
    const list = state.workspaces;
    if (list.project !== project) state.workspaces = { project, list: [], loaded: false, error: null };
    renderLanes();
    let workspaces;
    try {
      ({ workspaces } = await api('GET', `/v1/workspaces?project=${enc(project)}`));
    } catch (e) {
      if (state.workspaces.project !== project) return;
      state.workspaces.error = e.message;
      workspacesChanged();
      return;
    }
    if (state.workspaces.project !== project) return;
    state.workspaces = { project, list: workspaces, loaded: true, error: null };
    workspacesChanged();
    loadProjectWork(project, workspaces);
    if (workspaces.some((w) => workspaceLane(w).key === 'running')) {
      workspacesTimer = setTimeout(() => {
        if (state.source.kind === 'project' && state.source.id === project) loadWorkspaces(project);
      }, 3000);
    }
  }

  // What the project's workspaces ask of a person (the server's decisions)
  // and the sessions running in them (for the board), read together after
  // each workspace list.
  async function loadProjectWork(project, workspaces) {
    const current = () => state.source.kind === 'project' && state.source.id === project;
    const open = workspaces.filter((w) => ['ready', 'running'].includes(w.state));
    const [decisions, ...sessions] = await Promise.all([
      api('GET', `/v1/projects/${enc(project)}/decisions`).then((r) => r.decisions, () => null),
      ...open.map((w) => api('GET', `${workspacePath(w.id)}/sessions`).then((r) => r.sessions.map((s) => ({ ...s, workspace: w })), () => [])),
    ]);
    if (!current()) return;
    if (decisions) state.projectDecisions = { project, list: decisions };
    state.projectSessions = { project, list: sessions.flat().filter(sessionActive) };
    branchesChanged();
    updateProjectGlance();
  }

  // The listed project's decisions, once it is the listed source.
  const projectCards = () => (state.source.kind === 'project' && state.projectDecisions.project === state.source.id ? state.projectDecisions.list : []);

  function workspacesChanged() {
    renderLanes();
    if (state.route.name === 'project' || (state.route.name === 'home' && state.source.kind === 'project')) updateProjectGlance();
  }

  // Show a project in the sidebar, loading its workspaces when it was not.
  function useProject(id) {
    if (state.source.kind !== 'project' || state.source.id !== id) useSource({ kind: 'project', id });
    if (state.workspaces.project !== id) loadWorkspaces(id);
  }

  async function switchRepo(repo) {
    state.repo = repo;
    save(REPO_KEY, repo);
    state.branches = new Map();
    state.lastActive = new Map();
    state.tracks = new Map();
    state.answered = [];
    state.cursor = null;
    state.loaded = false;
    state.branchError = null;
    state.permissions = [];
    state.inbox = [];
    state.approvals = [];
    state.effects = [];
    if (state.stream) state.stream.abort();
    openStream();
    await loadBranches();
  }

  async function loadBranches() {
    const repo = state.repo;
    state.branchError = null;
    renderLanes();
    let branches;
    try {
      ({ branches } = await api('GET', `${repoPath()}/branches`));
    } catch (e) {
      if (e.status === 401) throw e;
      if (repo !== state.repo) return;
      state.branchError = e.message;
      branchesChanged();
      return;
    }
    if (repo !== state.repo) return;
    state.branches = new Map(branches.map((b) => [b.name, b]));
    state.loaded = true;
    refreshDecisions();
    refreshApprovals();
    branchesChanged();
    if (state.route.name === 'queue') renderQueue();
  }

  const refreshing = new Map();
  function refreshBranch(name) {
    if (refreshing.has(name)) return;
    refreshing.set(name, setTimeout(async () => {
      refreshing.delete(name);
      try {
        const info = await api('GET', branchPath(name));
        state.branches.set(name, info);
      } catch (e) {
        if (e.status === 404) state.branches.delete(name);
      }
      branchesChanged();
    }, 250));
  }

  function openStream() {
    const controller = new AbortController();
    state.stream = controller;
    const repo = state.repo;
    const url = `${repoPath(repo)}/events/stream${state.cursor != null ? `?cursor=${state.cursor}` : ''}`;
    setLive('reconnecting', 'connecting');
    (async () => {
      let response;
      try {
        response = await fetch(url, {
          headers: { Authorization: `Bearer ${state.token}`, Accept: 'text/event-stream' },
          signal: controller.signal, cache: 'no-store', credentials: 'omit',
        });
      } catch (_) {
        return retry(controller);
      }
      if (response.status === 401) { signOut('This token is no longer accepted (expired or revoked). Pair again.'); return; }
      if (response.status === 400) {
        // A cursor past the feed's end (a reset store): start again from now.
        state.cursor = null;
        loadBranches().catch(() => {});
        return retry(controller, 0);
      }
      if (!response.ok || !response.body) return retry(controller);
      state.backoff = 1000;
      setLive('live', 'live');
      const reader = response.body.getReader();
      const decoder = new TextDecoder();
      let buffer = '';
      try {
        for (;;) {
          const { value, done } = await reader.read();
          if (done) break;
          buffer += decoder.decode(value, { stream: true }).replace(/\r\n?/g, '\n');
          let at;
          while ((at = buffer.indexOf('\n\n')) >= 0) {
            const block = buffer.slice(0, at);
            buffer = buffer.slice(at + 2);
            if (controller === state.stream) onSse(block);
          }
        }
      } catch (_) { /* dropped: reconnect from the cursor */ }
      retry(controller);
    })();
  }

  function retry(controller, delay) {
    if (controller !== state.stream || controller.signal.aborted || !state.token) return;
    setLive('reconnecting', 'reconnecting');
    const wait = delay != null ? delay : state.backoff;
    state.backoff = Math.min(state.backoff * 2, 30000);
    setTimeout(() => { if (controller === state.stream) openStream(); }, wait);
  }

  function onSse(block) {
    let event = 'message';
    const data = [];
    for (const line of block.split('\n')) {
      if (!line || line.startsWith(':')) continue;
      const colon = line.indexOf(':');
      const field = colon < 0 ? line : line.slice(0, colon);
      const value = colon < 0 ? '' : line.slice(colon + 1).replace(/^ /, '');
      if (field === 'event') event = value;
      else if (field === 'data') data.push(value);
    }
    let payload;
    try { payload = JSON.parse(data.join('\n')); } catch (_) { return; }
    if (event === 'open') {
      const first = state.cursor == null;
      state.cursor = payload.cursor;
      // Anything recorded between the branch list and the stream's start.
      if (first && state.loaded) loadBranches().catch(() => {});
      return;
    }
    if (event !== 'activity') return;
    state.cursor = payload.seq;
    onActivity(payload);
  }

  function onActivity(entry) {
    const a = entry.activity || {};
    state.lastActive.set(entry.branch, entry.at_ms);
    if (a.harness && a.harness.type === 'permission_requested') {
      state.permissions.unshift({ branch: entry.branch, tool: a.harness.request.tool, at_ms: entry.at_ms, decision: null });
      state.permissions.length = Math.min(state.permissions.length, 50);
    } else if (a.decision) {
      const p = state.permissions.find((x) => x.branch === entry.branch && x.decision == null);
      if (p) p.decision = a.decision.allowed ? 'allowed' : 'denied';
    }
    if (a.message) refreshDecisions();
    if (a.effect || a.status || a.snapshot) refreshApprovals();
    if (state.loaded && Date.now() - entry.at_ms < 60000) {
      const n = notice(entry);
      // On screen the branch list is the notice; a hidden tab gets the system's.
      if (n && document.hidden && 'Notification' in window && Notification.permission === 'granted' && !state.pushOn) {
        try { new Notification(`${state.repo}: ${n.text}`, { tag: n.href }); } catch (_) { /* page notifications unavailable */ }
      }
    }
    refreshBranch(entry.branch);
    if (view && view.name === entry.branch) refreshTimeline(view);
  }

  // ---------------------------------------------------------------
  // Routing

  // Inbox and Approvals became Needs you; push notifications still open them.
  const ALIASES = { '': 'home', inbox: 'attention', approvals: 'attention' };
  const VIEWS = {
    home: renderHome,
    branch: renderBranch,
    new: renderNew,
    attention: () => {
      // The project's cards are read again on arrival; the repository's
      // are kept current by its stream.
      if (state.source.kind === 'project') loadWorkspaces(state.source.id);
      return renderAttention();
    },
    triggers: renderTriggers,
    queue: renderQueue,
    settings: renderSettings,
    newProject: renderNewProject,
    project: renderProject,
    workspace: renderWorkspace,
  };

  function parseRoute() {
    const hash = location.hash || '#/';
    const parts = hash.replace(/^#\/?/, '').split('/').map((p) => { try { return decodeURIComponent(p); } catch (_) { return p; } });
    if (parts[0] === 'b') return { name: 'branch', repo: parts[1], branch: parts.slice(2).join('/') };
    if (parts[0] === 'p' && parts[1] === 'new') return { name: 'newProject' };
    if (parts[0] === 'p' && parts[1]) return { name: 'project', project: parts[1], creating: parts[2] === 'new' };
    if (parts[0] === 'w' && parts[1]) return { name: 'workspace', workspace: parts[1], session: parts[2] === 's' && parts[3] ? parts[3] : null };
    const name = ALIASES[parts[0]] || parts[0];
    return { name: VIEWS[name] && !['branch', 'newProject', 'project', 'workspace'].includes(name) ? name : 'home' };
  }

  async function route() {
    if (!state.token) return;
    closePreview();
    state.route = parseRoute();
    const r = state.route;
    if (r.name === 'branch' && r.repo && state.repos.includes(r.repo)) {
      if (state.source.kind !== 'repo') useSource({ kind: 'repo' });
      if (r.repo !== state.repo) { await switchRepo(r.repo); renderPicker(); }
    }
    if (r.name === 'project') useProject(r.project);
    if (r.name !== 'workspace' || (wsView && wsView.id !== r.workspace)) closeWorkspace();
    // On a phone a project's glance is its workspace list, the sidebar.
    document.body.dataset.view = r.name === 'project' && !r.creating ? 'home' : r.name;
    view = null;
    drawn = null;
    const current = r.name === 'project' && r.creating ? 'newWorkspace' : r.name;
    for (const link of document.querySelectorAll('#sidebar a[data-route]')) {
      if (link.dataset.route === current) link.setAttribute('aria-current', 'page');
      else link.removeAttribute('aria-current');
    }
    renderLanes();
    await VIEWS[r.name]();
    const field = r.name === 'project' && r.creating ? $('workspace-title') : null;
    if (field) field.focus(); else focusHeading();
  }

  // A live update redraws a page only when what it shows changed, so focus
  // and half-typed text survive the stream.
  let drawn = null;
  function redraw(signature, draw) {
    if (signature === drawn) return;
    drawn = signature;
    draw();
  }

  function branchesChanged() {
    renderLanes();
    if (state.route.name === 'home' && state.source.kind === 'repo') redraw(homeSignature(), renderHome);
    if (state.route.name === 'attention') redraw(attentionSignature(), renderAttention);
    if (state.route.name === 'branch') updateBranch();
  }

  // ---------------------------------------------------------------
  // The sidebar: a repository's branches, or a project's workspaces, by lane

  function renderLanes() {
    const list = $('branch-list');
    const needs = lanes(Array.from(state.branches.values()))[0].branches.length + projectCards().length;
    document.title = needs ? `(${needs}) Branchyard` : 'Branchyard';
    $('attention-count').hidden = !needs;
    $('attention-count').textContent = String(needs);
    const focused = list.contains(document.activeElement) ? document.activeElement.dataset.key : null;
    const needle = state.filter.trim().toLowerCase();
    if (state.source.kind === 'none') { $('filter').hidden = true; fill(list); return; }
    fill(list, ...(state.source.kind === 'project' ? workspaceLanes(needle) : branchLanes(needle)));
    if (focused) {
      const again = Array.from(list.querySelectorAll('a.branch-row')).find((a) => a.dataset.key === focused);
      if (again) again.focus({ preventScroll: true });
    }
  }

  // A lane of rows: a heading and its count, or, when collapsed, a
  // disclosure that a search opens.
  function laneGroup(lane, rows, needle, prefix) {
    if (!rows.length) return null;
    const id = `${prefix}${lane.key}`;
    const items = h('ul', { class: 'lane-list' }, rows);
    if (!lane.collapsed) {
      return h('section', { class: 'lane', id, 'aria-labelledby': `${id}-h` },
        h('h2', { class: 'lane-head', id: `${id}-h` }, h('span', { text: lane.label }), h('span', { class: 'lane-count', text: String(rows.length) })),
        items);
    }
    const open = state.doneOpen || !!needle;
    const summary = h('summary', { class: 'lane-head' }, h('span', { text: `${open ? 'Hide' : 'Show'} ${rows.length} ${lane.label.toLowerCase()}` }));
    const box = h('details', { class: 'lane', id, open }, summary, items);
    box.addEventListener('toggle', () => {
      if (!needle) state.doneOpen = box.open;
      summary.firstChild.textContent = `${box.open ? 'Hide' : 'Show'} ${rows.length} ${lane.label.toLowerCase()}`;
    });
    return box;
  }

  function noMatches(hint) {
    return h('section', { class: 'list-state' },
      h('h2', { text: `No matching ${state.source.kind === 'project' ? 'workspaces' : 'branches'}` }),
      h('p', { class: 'muted', text: hint }),
      h('button', { type: 'button', text: 'Clear search', onclick: () => {
        const search = $('branch-filter');
        state.filter = '';
        search.value = '';
        renderLanes();
        search.focus();
      } }));
  }

  function branchLanes(needle) {
    const all = Array.from(state.branches.values());
    const matches = (b) => !needle || b.name.toLowerCase().includes(needle) || (b.prompt || '').toLowerCase().includes(needle);
    const shown = all.filter(matches);
    const waiting = attention();
    $('filter').hidden = !all.length;
    $('branch-count').textContent = needle
      ? `Showing ${shown.length} of ${all.length} branches`
      : plural(all.length, 'branch', 'branches');
    const current = state.route.name === 'branch' ? state.route.branch : null;
    const row = (b, lane) => h('li', {}, h('a', {
      class: 'branch-row', href: branchHref(b.name), 'data-branch': b.name, 'data-key': `b:${b.name}`, title: b.name,
      'aria-current': b.name === current ? 'page' : null,
    }, dot(lane.key), h('span', { class: 'row-name', text: b.name }),
    h('span', { class: 'row-meta' }, lane.meta(b, waiting), lane.key === 'needs' || lane.key === 'running' ? [' · ', ago(activeAt(b), '')] : null)));
    const status = [];
    if (state.branchError) {
      status.push(h('section', { class: 'list-state error-state', role: 'alert' },
        h('h2', { text: state.loaded ? 'Could not refresh branches' : 'Could not load branches' }),
        h('p', { text: state.branchError }),
        h('p', { class: 'hint', text: 'Check that the server is reachable and this token can read the repository, then retry. Your token stays in this tab.' }),
        h('button', { type: 'button', text: 'Retry loading branches', onclick: async () => {
          try { await loadBranches(); } catch (e) {
            if (e.status !== 401) toast(`Could not load branches: ${e.message}`);
          }
        } })));
    } else if (!all.length) {
      status.push(h('section', { class: 'list-state', role: state.loaded ? null : 'status' },
        h('h2', { text: state.loaded ? 'No branches yet.' : 'Loading branches…' }),
        h('p', { class: 'muted', text: state.loaded
          ? can('run') ? 'Start a task to create its own branch. Its progress and results will appear here.'
            : 'Branches started from the CLI or another browser will appear here. This token has read access.'
          : 'Getting the latest work from your server.' })));
    }
    if (all.length && !shown.length) status.push(noMatches('Try another name or a word from the prompt.'));
    return [...status, ...lanes(shown).map(({ lane, branches }) => laneGroup(lane, branches.map((b) => row(b, lane)), needle, 'lane-'))];
  }

  function workspaceLanes(needle) {
    const { list: all, loaded, error } = state.workspaces;
    const shown = all.filter((w) => !needle || w.title.toLowerCase().includes(needle));
    $('filter').hidden = !all.length;
    $('branch-count').textContent = needle
      ? `Showing ${shown.length} of ${all.length} workspaces`
      : plural(all.length, 'workspace');
    const current = state.route.name === 'workspace' ? state.route.workspace : null;
    const row = (w, lane) => h('li', {}, h('a', {
      class: 'branch-row', href: `#/w/${enc(w.id)}`, 'data-workspace': w.id, 'data-key': `w:${w.id}`, title: w.title,
      'aria-current': w.id === current ? 'page' : null,
    }, dot(lane.key), h('span', { class: 'row-name', text: w.title }), h('span', { class: 'row-meta', text: workspaceStateText(w) })));
    const status = [];
    if (error) {
      status.push(h('section', { class: 'list-state error-state', role: 'alert' },
        h('h2', { text: loaded ? 'Could not refresh workspaces' : 'Could not load workspaces' }),
        h('p', { text: error }),
        h('button', { type: 'button', text: 'Retry loading workspaces', onclick: () => loadWorkspaces(state.source.id) })));
    } else if (!all.length) {
      status.push(h('section', { class: 'list-state', role: loaded ? null : 'status' },
        h('h2', { text: loaded ? 'No workspaces yet.' : 'Loading workspaces…' }),
        h('p', { class: 'muted', text: loaded
          ? can('run') ? 'A workspace holds sources, sessions and their outputs. Start one with New workspace.' : 'Workspaces you are a member of appear here.'
          : 'Getting this project\'s workspaces.' })));
    }
    if (all.length && !shown.length) status.push(noMatches('Try another word from a workspace title.'));
    const sorted = shown.slice().sort((x, y) => y.created_at_ms - x.created_at_ms);
    return [...status, ...WORKSPACE_LANES.map((lane) => laneGroup(lane, sorted.filter((w) => workspaceLane(w) === lane).map((w) => row(w, lane)), needle, 'ws-lane-'))];
  }

  // `j` and `k` walk the visible branches; Enter follows the focused link.
  function stepBranch(by) {
    const rows = Array.from(document.querySelectorAll('#branch-list a.branch-row')).filter((a) => a.offsetParent);
    if (!rows.length) return;
    let at = rows.indexOf(document.activeElement);
    if (at < 0) at = rows.findIndex((a) => a.getAttribute('aria-current') === 'page');
    const next = at < 0 ? (by > 0 ? 0 : rows.length - 1) : Math.min(rows.length - 1, Math.max(0, at + by));
    rows[next].focus();
  }

  // ---------------------------------------------------------------
  // Home: the repository at a glance

  function homeSignature() {
    const groups = lanes(Array.from(state.branches.values()));
    return JSON.stringify([state.loaded, state.branchError, Math.floor(Date.now() / 30000),
      groups.map((g) => g.branches.map((b) => [b.name, b.status, b.stalled, b.awaiting, b.cost_usd, activeAt(b)])), Array.from(attention()), state.steering]);
  }

  // The board's clock: the smallest of these windows that holds the
  // oldest thing it draws, now at the right edge. A yard whose work began
  // minutes ago is drawn over minutes, not lost in an hour. Each window's
  // quarters are round numbers.
  const MINUTE = 60 * 1000;
  const BOARD_WINDOWS = [4, 8, 20, 40, 60, 120, 240, 480, 720, 1440].map((m) => m * MINUTE);
  function boardScale(oldest, now) {
    const span = Math.max(0, now - oldest) * 1.06;
    const win = BOARD_WINDOWS.find((w) => w >= span) || BOARD_WINDOWS[BOARD_WINDOWS.length - 1];
    const x = (ms) => Math.max(0, Math.min(100, ((ms - (now - win)) / win) * 100));
    const minus = (ms) => {
      const m = Math.round(ms / MINUTE);
      return m < 60 ? `−${m}m` : `−${Math.floor(m / 60)}h${m % 60 ? `${m % 60}m` : ''}`;
    };
    const label = win < 60 * MINUTE ? `last ${win / MINUTE} min` : win < 1440 * MINUTE ? `last ${plural(win / 60 / MINUTE, 'hour')}` : 'last day';
    return { window: win, now, x, label, ticks: [1, 0.75, 0.5, 0.25].map((f) => minus(win * f)) };
  }
  const place = (el, scale, from, to) => {
    const left = scale.x(from);
    el.style.left = `${left}%`;
    el.style.width = `${Math.max(0.4, scale.x(to) - left)}%`;
    return el;
  };
  const at = (el, scale, ms) => { el.style.left = `${scale.x(ms)}%`; return el; };

  // A branch's turns from its latest events: when each began and ended,
  // when each tool call started, and when the branch merged.
  function trackOf(events) {
    const turns = [];
    let turn = null;
    let merged = null;
    for (const { activity: a, at_ms: ms } of events) {
      const ev = a.harness || {};
      if (a.prompt != null || a.steered) {
        if (turn && turn.end == null && a.steered) continue;
        turn = { start: ms, end: null, tools: [] };
        turns.push(turn);
      } else if (ev.type === 'tool_started') {
        if (!turn) { turn = { start: ms, end: null, tools: [] }; turns.push(turn); }
        turn.tools.push(ms);
      } else if (a.status) {
        if (a.status.state === 'merged') merged = ms;
        if (turn && turn.end == null && !['running', 'waiting'].includes(a.status.state)) turn.end = ms;
      }
    }
    return { turns, merged };
  }

  // Read the tracks the board draws, each once per change of its branch:
  // a branch's activity marks its track stale, and the next home redraw
  // reads it again.
  const trackKey = (b) => JSON.stringify([b.status, b.turns, activeAt(b)]);
  function trackFor(b) {
    const key = trackKey(b);
    const found = state.tracks.get(b.name);
    if (found && (found.key === key || found.loading)) return found.track;
    const entry = { key, loading: true, track: found ? found.track : null };
    state.tracks.set(b.name, entry);
    const repo = state.repo;
    api('GET', `${branchPath(b.name)}/event-page?limit=200`)
      .then((page) => { entry.track = trackOf(page.events); })
      .catch(() => {})
      .finally(() => {
        entry.loading = false;
        if (repo === state.repo) branchesChanged();
      });
    return entry.track;
  }

  function homeSignature() {
    const groups = lanes(Array.from(state.branches.values()));
    return JSON.stringify([state.loaded, state.branchError, Math.floor(Date.now() / 15000),
      groups.map((g) => g.branches.map((b) => [b.name, b.status, b.stalled, b.awaiting, b.cost_usd, activeAt(b)])), Array.from(attention()), state.steering,
      Array.from(state.tracks.entries()).map(([k, t]) => [k, t.loading, t.track && t.track.turns.length]), state.answered.length]);
  }

  // One branch on the clock: its name and what it is doing, its turns on
  // the rail (a turn a bar, each tool call a tick on it, a wait on you
  // filled amber from when it parked, a merge a green tick), what it cost
  // and the one thing to do with it. The row opens the branch.
  function boardRow(b, lane, scale) {
    const s = statusOf(b);
    const now = scale.now;
    const track = trackFor(b) || { turns: [{ start: b.created_at * 1000, end: lane.key === 'running' ? null : activeAt(b), tools: [] }], merged: null };
    const open = !['merged', 'no_changes', 'discarded'].includes(b.status.state);
    const rail = h('span', { class: 'rail' });
    const first = track.turns.length ? track.turns[0].start : b.created_at * 1000;
    if (first < now - scale.window) rail.append(h('i', { class: 'rail-before', title: `started ${clock(first)}` }));
    const calls = track.turns.reduce((n, t) => n + t.tools.length, 0);
    track.turns.forEach((t, i) => {
      const last = i === track.turns.length - 1;
      const end = t.end != null ? t.end : (open && last ? now : activeAt(b));
      rail.append(place(h('i', { class: `seg seg-${lane.key}${t.end == null && open && last ? ' seg-live' : ''}`, title: `turn ${i + 1}: ${clock(t.start)}${t.end != null ? `–${clock(t.end)}` : ', running'} · ${plural(t.tools.length, 'call')}` }), scale, t.start, end));
      for (const ms of t.tools) rail.append(at(h('i', { class: 'call' }), scale, ms));
    });
    for (const a of state.answered.filter((x) => x.branch === b.name && x.answer)) {
      rail.append(place(h('i', { class: 'wait wait-past', title: `waited on you ${clock(a.created_ms)}–${clock(a.answer.at_ms)}, ${a.answer.allow ? 'allowed' : 'denied'}` }), scale, a.created_ms, a.answer.at_ms));
    }
    if (b.awaiting) rail.append(place(h('i', { class: 'wait', title: `waiting for you since ${clock(b.awaiting.since_ms)}` }), scale, b.awaiting.since_ms, now));
    const lastTurn = track.turns[track.turns.length - 1];
    if (lane.key === 'ready' && lastTurn && lastTurn.end) rail.append(place(h('i', { class: 'rail-ready', title: 'ready, waiting to be merged' }), scale, lastTurn.end, now));
    if (track.merged) rail.append(at(h('i', { class: 'tick-merge', title: `merged ${clock(track.merged)}` }), scale, track.merged));
    if (lane.key === 'needs' && !b.awaiting && lastTurn && lastTurn.end) rail.append(at(h('i', { class: 'stop', title: `${s.text} ${clock(lastTurn.end)}` }), scale, lastTurn.end));
    else if (open) rail.append(h('i', { class: `head head-${lane.key}` }));
    let meta;
    if (b.awaiting) meta = [h('b', { text: 'waiting for you' }), ` · ${since(b.awaiting.since_ms)}`];
    else if (lane.key === 'running') meta = b.status.state === 'running' ? `${b.stalled ? 'silent' : 'working'} · turn ${b.turns + 1} · ${plural(calls, 'call')}` : s.text;
    else if (lane.key === 'ready') meta = ['ready · ', b.candidate ? counts(b.candidate.insertions, b.candidate.deletions) : null, ` · ${plural(b.turns, 'turn')}`];
    else if (lane.key === 'done') meta = [s.text, track.merged ? ` · ${clock(track.merged).slice(0, 5)}` : ''];
    else meta = [s.text, calls ? ` after ${plural(calls, 'call')}` : ''];
    let action = null;
    if (b.awaiting || attention().has(b.name)) action = h('a', { class: 'button small needs', href: '#/attention', text: 'Answer' });
    else if (lane.key === 'ready' && applies(ACTIONS.merge, b)) action = h('button', { type: 'button', class: 'small primary', text: 'Merge', onclick: () => runAction(ACTIONS.merge, b.name) });
    else if (lane.key === 'running' && applies(COMPOSER.steer, b)) action = h('button', { type: 'button', class: 'small', text: 'Steer', 'aria-pressed': String(state.steering === b.name), onclick: () => { state.steering = state.steering === b.name ? null : b.name; redraw(homeSignature(), renderHome); } });
    else if (lane.key === 'needs' && applies(ACTIONS.resume, b)) action = h('button', { type: 'button', class: 'small', text: 'Resume', onclick: () => runAction(ACTIONS.resume, b.name) });
    else action = h('a', { class: 'button small', href: branchHref(b.name), text: 'Open' });
    const row = h('div', { class: `board-row board-${lane.key}${b.awaiting ? ' board-parked' : ''}`, 'data-branch': b.name },
      h('span', { class: 'board-name' }, dot(lane.key),
        h('span', { class: 'board-id' }, h('a', { href: branchHref(b.name), text: b.name }), h('span', { class: 'board-meta' }, meta))),
      rail,
      h('span', { class: `board-cost mono${b.cost_usd == null ? ' muted' : ''}`, title: b.cost_usd == null ? 'its harness reports no cost' : `what ${b.harness} reported spending` }, cents(b.cost_usd)),
      h('span', { class: 'board-act' }, action));
    row.addEventListener('click', (ev) => {
      if (ev.target.closest('a, button, input')) return;
      location.hash = branchHref(b.name);
    });
    if (state.steering === b.name) return [row, steerRow(b)];
    return row;
  }

  // One line under a running row: say something to the turn without
  // opening the branch.
  function steerRow(b) {
    const field = h('input', { type: 'text', id: `steer-${b.name}`, placeholder: `Say something to ${b.name} while it works…`, autocomplete: 'off' });
    const send = async () => {
      const text = field.value.trim();
      if (!text) { field.focus(); return; }
      field.disabled = true;
      const result = await act(COMPOSER.steer, b.name, text);
      if (result) { state.steering = null; redraw(homeSignature(), renderHome); } else field.disabled = false;
    };
    field.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') { ev.preventDefault(); send(); } if (ev.key === 'Escape') { state.steering = null; redraw(homeSignature(), renderHome); } });
    const row = h('div', { class: 'board-steer' },
      h('label', { class: 'visually-hidden', for: `steer-${b.name}`, text: `Steer ${b.name}` }), field,
      h('button', { type: 'button', class: 'small', text: 'Steer', onclick: send }));
    setTimeout(() => field.focus(), 0);
    return row;
  }

  // The axis over the rails: where the window starts, its quarters, and
  // now, with the time.
  function boardAxis(scale, name) {
    return h('div', { class: 'board-axis', 'aria-hidden': 'true' },
      h('span', { class: 'board-name' }, name, h('span', { class: 'board-window', text: scale.label })),
      h('span', { class: 'rail-axis' }, scale.ticks.map((t) => h('span', { text: t })),
        h('span', { class: 'axis-now' }, 'now ', h('b', { text: clock(scale.now).slice(0, 5) }))),
      h('span', { class: 'board-cost', text: 'Cost' }),
      h('span', { class: 'board-act' }));
  }

  // A number with its meaning: the yard's summary tiles.
  function tile(key, label, value, meaning, href) {
    const body = [h('span', { class: 'tile-label' }, dot(key), label), h('b', { class: 'tile-value', text: value }), h('span', { class: 'tile-meaning', title: typeof meaning === 'string' ? meaning : null }, meaning)];
    return h('li', { class: `tile tile-${key}` }, href ? h('a', { class: 'tile-link', href }, body) : h('div', { class: 'tile-body' }, body));
  }

  // Under the board: what was decided today, and where the money went,
  // each branch's reported cost as a bar against the largest.
  function homeLower(all, groups) {
    const recent = recentDecisions();
    const spent = all.filter((b) => b.cost_usd != null).sort((x, y) => y.cost_usd - x.cost_usd);
    const top = spent.length ? spent[0].cost_usd : 0;
    const laneKey = (b) => groups.find((g) => g.branches.includes(b)).lane.key;
    return h('div', { class: 'home-lower' },
      h('section', { class: 'recent-box', 'aria-labelledby': 'home-recent-h' },
        h('h2', { id: 'home-recent-h' }, 'Decided today', recent.length ? h('span', { class: 'lane-count', text: String(recent.length) }) : null),
        recent.length ? recentList(recent) : h('p', { class: 'muted recent-none', text: 'Nothing allowed, denied or merged yet today.' })),
      h('section', { class: 'recent-box', 'aria-labelledby': 'home-spend-h' },
        h('h2', { id: 'home-spend-h', text: 'Where the money went' }),
        spent.length ? h('ul', { class: 'spend' }, spent.slice(0, 6).map((b) => {
          const bar = h('i');
          bar.style.width = `${top ? Math.max(2, (b.cost_usd / top) * 100) : 0}%`;
          return h('li', { class: `spend-${laneKey(b)}` },
            h('a', { class: 'spend-name', href: branchHref(b.name), text: b.name }),
            h('span', { class: 'spend-track', title: `${b.name}: ${money(b.cost_usd)}` }, bar),
            h('span', { class: 'spend-cost', text: cents(b.cost_usd) }));
        }), h('li', { class: 'spend-total' }, h('span', { text: spent.length > 6 ? `${spent.length - 6} more` : 'In all' }), h('span'),
          h('span', { class: 'spend-cost', text: cents(spent.reduce((n, b) => n + b.cost_usd, 0)) })))
          : h('p', { class: 'muted recent-none', text: 'No branch here reports what it spends. A harness that reports cost, such as Claude Code, shows it here.' })));
  }

  function renderHome() {
    if (state.source.kind === 'project') return renderProject();
    if (state.source.kind === 'none') return renderNoSource();
    const all = Array.from(state.branches.values());
    const groups = lanes(all);
    const needs = groups[0].branches;
    const ready = groups[2].branches;
    const done = groups[3].branches;
    const waiting = attention();
    const now = Date.now();
    const midnight = new Date(now).setHours(0, 0, 0, 0);
    const reporting = all.filter((b) => b.cost_usd != null);
    const cost = reporting.reduce((sum, b) => sum + b.cost_usd, 0);
    const inflight = groups[1].branches;
    const parked = needs.filter((b) => b.awaiting || waiting.has(b.name)).length;
    const mergedToday = done.filter((b) => b.status.state === 'merged' && activeAt(b) >= midnight);
    const added = ready.reduce((n, b) => n + (b.candidate ? b.candidate.insertions : 0), 0);
    const removed = ready.reduce((n, b) => n + (b.candidate ? b.candidate.deletions : 0), 0);
    const lede = !state.loaded
      ? state.branchError ? 'The branch list did not load.' : 'Loading…'
      : !all.length ? 'Nothing has run here yet.'
        : [needs.length ? `${plural(needs.length, 'branch', 'branches')} ${needs.length === 1 ? 'needs' : 'need'} you` : 'Nothing needs you right now',
          `${inflight.length} in flight`, `${ready.length} ready`, reporting.length ? `${cents(cost)} spent so far` : null].filter(Boolean).join(' · ');
    const open = groups.slice(0, 3).flatMap((g) => g.branches);
    const starts = all.filter((b) => open.includes(b) || activeAt(b) >= now - 1440 * MINUTE).map((b) => {
      const t = trackFor(b);
      return t && t.turns.length ? t.turns[0].start : b.created_at * 1000;
    });
    const scale = boardScale(starts.length ? Math.min(...starts) : now, now);
    const tiles = state.loaded && all.length ? h('ul', { class: 'tiles', 'aria-label': 'The yard at a glance' },
      tile('running', 'In flight', String(inflight.length), inflight.length ? `${plural(inflight.filter((b) => b.status.state === 'running').length, 'agent')} working now` : 'no agent is working'),
      tile('needs', 'Need you', String(needs.length), needs.length ? [parked ? `${parked} parked on you` : null, parked && needs.length > parked ? ', ' : null, needs.length > parked ? `${needs.length - parked} stopped` : null] : 'nothing waits on you', needs.length ? '#/attention' : null),
      tile('ready', 'Ready', String(ready.length), ready.length ? ['to merge · ', counts(added, removed)] : 'nothing to merge'),
      tile('done', 'Merged today', String(mergedToday.length), mergedToday.length ? mergedToday.slice(0, 2).map((b) => b.name).join(', ') + (mergedToday.length > 2 ? ` +${mergedToday.length - 2}` : '') : `${plural(done.length, 'branch', 'branches')} done in all`),
      tile('cost', 'Spent', reporting.length ? cents(cost) : '—', reporting.length ? `by ${plural(reporting.length, 'branch', 'branches')} that report it` : 'no harness here reports cost')) : null;
    const doneCost = done.reduce((sum, b) => sum + (b.cost_usd || 0), 0);
    const doneGroup = done.length ? (() => {
      const ticks = h('span', { class: 'rail rail-done' }, done.map((b) => {
        const t = trackFor(b);
        const ms = (t && t.merged) || activeAt(b);
        return at(h('i', { class: b.status.state === 'merged' ? 'tick-merge' : 'tick-done', title: `${b.name}: ${statusOf(b).text}, ${clock(ms)}` }), scale, ms);
      }));
      const box = h('details', { class: 'board-done', open: state.boardDoneOpen },
        h('summary', { class: 'board-row board-done-head' },
          h('span', { class: 'board-name' }, h('span', { class: 'fold', 'aria-hidden': 'true' }),
            h('span', { class: 'board-id' }, h('span', { class: 'board-done-title', text: `Done · ${done.length}` }),
              h('span', { class: 'board-meta', text: done.map((b) => b.name).slice(0, 3).join(', ') + (done.length > 3 ? ` +${done.length - 3}` : '') }))),
          ticks,
          h('span', { class: 'board-cost mono muted', text: doneCost ? cents(doneCost) : '' }),
          h('span', { class: 'board-act' })),
        done.map((b) => boardRow(b, groups[3].lane, scale)));
      box.addEventListener('toggle', () => { state.boardDoneOpen = box.open; });
      return box;
    })() : null;
    main(h('div', { class: 'page full home' },
      h('header', { class: 'page-head' },
        h('div', {}, h('h1', { text: state.repo }), h('p', { class: 'lede', text: lede })),
        state.loaded && all.length && can('run') ? h('a', { class: 'button primary', href: '#/new' }, icon('plus', true), 'New task') : null),
      tiles,
      state.loaded && all.length ? h('section', { class: 'board', 'aria-label': 'Every branch on one clock' },
        boardAxis(scale, 'Branch'),
        open.length ? groups.slice(0, 3).map(({ lane, branches }) => branches.map((b) => boardRow(b, lane, scale)))
          : h('p', { class: 'board-calm' }, checkMark(false), 'Nothing is open. Every branch here is done.'),
        doneGroup) : null,
      state.loaded && all.length ? h('p', { class: 'board-legend', 'aria-label': 'How to read the board' },
        h('span', { class: 'key' }, h('i', { class: 'key-seg' }), 'a turn'),
        h('span', { class: 'key' }, h('i', { class: 'key-call' }), 'a tool call'),
        h('span', { class: 'key' }, h('i', { class: 'key-wait' }), 'waiting on you'),
        h('span', { class: 'key' }, h('i', { class: 'key-merge' }), 'merged'),
        needs.length ? h('span', { class: 'key-note' }, h('a', { href: '#/attention', text: 'Decide' }), ' takes what waits on you one at a time.') : null) : null,
      state.loaded && all.length ? homeLower(all, groups) : null,
      state.loaded && !all.length ? h('section', { class: 'empty' },
        h('span', { class: 'empty-mark', 'aria-hidden': 'true' }, icon('bolt')),
        h('h2', { text: 'An empty yard' }),
        h('p', { text: can('run') ? 'Start a task and it gets a branch of its own: its conversation, its changes and a merge button. Every branch then shows here on one clock.' : 'Branches started from the CLI or another browser show here on one clock. This token can read, not start.' }),
        can('run') ? h('div', { class: 'row' }, h('a', { class: 'button primary', href: '#/new', text: 'Start a task' }), h('span', { class: 'hint' }, 'or ', h('code', { text: 'by run "…"' }), ' from a terminal')) : null) : null));
  }

  // A token that reaches no repository, before its first project.
  function renderNoSource() {
    main(h('div', { class: 'page' },
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Projects' }),
        h('p', { class: 'lede', text: 'This token reaches no repository. A knowledge project needs none: it holds the sources you upload, the sessions that work on them, and the outputs you accept.' }))),
      can('run') ? h('div', { class: 'welcome' },
        h('p', { text: 'Start a project, then a workspace in it, and give its sessions your documents.' }),
        h('a', { class: 'button primary', href: '#/p/new', text: 'New project' }))
        : h('p', { class: 'muted', text: 'This token has read access only; projects started elsewhere appear in the picker.' })));
  }

  // ---------------------------------------------------------------
  // Projects: a new one, and one at a glance with its workspaces

  const RESULT_MODES = [
    ['artifacts', 'Outputs to accept'],
    ['mixed', 'Code and outputs'],
    ['code', 'Code changes'],
  ];
  const resultText = (mode) => (RESULT_MODES.find(([m]) => m === mode) || [mode, mode])[1].toLowerCase();
  const sourceText = (p) => (p.source.kind === 'knowledge' ? 'Knowledge project' : `Repository project on ${p.source.repo}`);

  function renderNewProject() {
    const nameId = nextId('project');
    const sourceId = nextId('source');
    const modeId = nextId('mode');
    const name = h('input', { id: nameId, type: 'text', autocomplete: 'off', required: true });
    const source = h('select', { id: sourceId },
      h('option', { value: 'knowledge', text: 'Knowledge work (no repository)' }),
      state.repos.map((r) => h('option', { value: `repo:${r}`, text: `Repository ${r}` })));
    const mode = h('select', { id: modeId }, RESULT_MODES.map(([value, text]) => h('option', { value, text })));
    const error = h('p', { class: 'error', role: 'alert' });
    const back = state.source.kind === 'project' ? `#/p/${enc(state.source.id)}` : '#/';
    const go = h('button', {
      type: 'button', class: 'primary', text: 'Create project', disabled: !can('run'),
      onclick: async () => {
        const title = name.value.trim();
        if (!title) { name.focus(); return; }
        const body = {
          name: title,
          source: source.value === 'knowledge' ? { kind: 'knowledge' } : { kind: 'repository', repo: source.value.slice(5) },
          result_mode: mode.value,
        };
        go.disabled = true;
        error.textContent = '';
        let project;
        try {
          project = await api('POST', '/v1/projects', body);
        } catch (e) {
          error.textContent = e.code === 'knowledge_disabled' ? `${e.message}: this server keeps no knowledge projects until its operator turns them on.` : e.message;
          go.disabled = false;
          return;
        }
        state.projects = (state.projects || []).filter((p) => p.id !== project.id).concat(project);
        toast(`Project ${project.name} created.`);
        location.hash = `#/p/${enc(project.id)}`;
      },
    });
    name.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') go.click(); });
    main(h('div', { class: 'page narrow' },
      h('a', { class: 'back', href: back, text: '‹ Back' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'New project' }),
        h('p', { class: 'lede', text: 'A project names where work comes from and what counts as its result. Its workspaces hold the sessions and their outputs.' }))),
      h('div', { class: 'card form' },
        h('label', { class: 'block', for: nameId, text: 'Project name' }), name,
        h('div', { class: 'form-grid' },
          h('div', {}, h('label', { class: 'block', for: sourceId, text: 'Source' }), source),
          h('div', {}, h('label', { class: 'block', for: modeId, text: 'Results' }), mode)),
        h('p', { class: 'hint', text: 'Knowledge work keeps its files on the server; a repository project works on branches of a repository this token reaches.' }),
        h('div', { class: 'row form-actions' }, go),
        can('run') ? null : h('p', { class: 'hint', text: 'This token lacks the run scope.' })),
      error));
  }

  // A project at a glance, as home is a repository's: counts per lane, a
  // field to start a workspace, and its workspaces.
  async function renderProject() {
    const route = state.route;
    const id = route.name === 'project' ? route.project : state.source.id;
    let project = projectOf(id);
    if (!project) {
      let error = null;
      try {
        project = await api('GET', `/v1/projects/${enc(id)}`);
        state.projects = (state.projects || []).filter((p) => p.id !== id).concat(project);
        renderPicker();
      } catch (e) {
        error = e;
      }
      if (state.route !== route) return;
      if (error) {
        main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Back' }),
          h('h1', { text: 'Project not found' }), h('p', { class: 'error', text: error.message })));
        return;
      }
    }
    const titleField = h('input', { id: 'workspace-title', type: 'text', autocomplete: 'off', placeholder: 'What is this workspace for?' });
    const status = h('p', { class: 'hint', role: 'status' });
    const create = h('button', {
      type: 'button', class: 'primary', text: 'Create workspace', disabled: !can('run'),
      onclick: () => createWorkspace(project, titleField, create, status),
    });
    titleField.addEventListener('keydown', (ev) => { if (ev.key === 'Enter') create.click(); });
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: `#/p/${enc(project.id)}`, text: '‹ Workspaces' }),
      h('header', { class: 'page-head' }, h('div', {},
        h('h1', { text: project.name }),
        h('p', { class: 'lede', id: 'project-lede' }))),
      h('ul', { class: 'glance', id: 'project-glance', 'aria-label': 'Workspaces by state' }),
      h('section', { class: 'board', id: 'project-board', 'aria-label': 'Everything open in this project on one clock', hidden: true }),
      can('run') ? h('section', { class: 'card new-workspace', 'aria-label': 'New workspace' },
        h('label', { class: 'block', for: 'workspace-title', text: 'Workspace title' }),
        h('div', { class: 'row inline-form' }, titleField, create),
        status) : null,
      h('h2', { text: 'Workspaces' }),
      h('div', { id: 'project-cards' })));
    updateProjectGlance();
  }

  // What the glance shows from the workspace list; a poll redraws only this,
  // so a title being typed stays.
  function updateProjectGlance() {
    const lede = $('project-lede');
    if (!lede) return;
    const project = projectOf(state.workspaces.project);
    const { list, loaded, error } = state.workspaces;
    if (!project) return;
    lede.textContent = `${sourceText(project)} · ${resultText(project.result_mode)} · ${loaded ? plural(list.length, 'workspace') : 'loading workspaces…'}`;
    fill($('project-glance'), loaded && list.length ? WORKSPACE_LANES.map((lane) => {
      const n = list.filter((w) => workspaceLane(w) === lane).length;
      return h('li', { class: 'glance-item' }, h('span', { class: 'glance-label' }, dot(n ? lane.key : 'done'), lane.label), h('b', { text: String(n) }));
    }) : null);
    updateProjectBoard();
    const sorted = list.slice().sort((x, y) => y.created_at_ms - x.created_at_ms);
    fill($('project-cards'), error ? h('p', { class: 'error', text: error })
      : !loaded ? h('p', { class: 'muted', text: 'Loading…' })
        : !list.length ? h('p', { class: 'muted', text: 'No workspaces yet.' })
          : h('ul', { class: 'cards' }, sorted.map((w) => {
            const lane = workspaceLane(w);
            return h('li', {}, h('a', { class: 'card card-link', href: `#/w/${enc(w.id)}` },
              h('span', { class: 'card-head' }, dot(lane.key), h('span', { class: 'card-name', text: w.title }), ago(w.created_at_ms)),
              h('span', { class: 'card-text', text: cap(workspaceStateText(w)) }),
              w.error ? h('span', { class: 'card-prompt', text: w.error.message }) : null));
          })));
  }

  // Everything open in the project, on the board's clock: a row for each
  // session running or waiting, each output delivered and waiting to be
  // accepted, and each session that stopped. The same decisions the deck
  // takes one at a time.
  function updateProjectBoard() {
    const board = $('project-board');
    if (!board) return;
    const id = state.workspaces.project;
    const running = state.projectSessions.project === id ? state.projectSessions.list : [];
    const cards = state.projectDecisions.project === id ? state.projectDecisions.list : [];
    const now = Date.now();
    const rows = [
      ...cards.filter((d) => d.kind === 'session').map((d) => ({ lane: 'needs', d })),
      ...running.map((s) => ({ lane: 'running', s })),
      ...cards.filter((d) => d.kind === 'output').map((d) => ({ lane: 'ready', d })),
    ];
    board.hidden = !rows.length;
    if (!rows.length) { fill(board); return; }
    const startOf = ({ d, s }) => (s ? s.created_at_ms : d.kind === 'output' ? d.since_ms : now);
    const scale = boardScale(Math.min(...rows.map(startOf)), now);
    const row = ({ lane, d, s }) => {
      const started = s ? s.created_at_ms : d.kind === 'output' ? d.since_ms : now;
      const name = s ? s.name : d.session_name;
      const where = s ? s.workspace.title : d.workspace_title;
      const href = s ? workspaceHref(s.workspace.id, s.id) : workspaceHref(d.workspace, d.session);
      const meta = s ? `${where} · ${sessionPhase(s).label.toLowerCase()}` : d.kind === 'output' ? `${where} · delivered ${d.output.name}` : `${where} · ${d.phase.toLowerCase()}`;
      const rail = h('span', { class: 'rail' });
      if (started < now - scale.window) rail.append(h('i', { class: 'rail-before', title: `started ${clock(started)}` }));
      if (lane === 'ready') rail.append(place(h('i', { class: 'rail-ready', title: 'delivered, waiting to be accepted' }), scale, started, now));
      else rail.append(place(h('i', { class: `seg seg-${lane}${lane === 'running' ? ' seg-live' : ''}`, title: `since ${clock(started)}` }), scale, started, now));
      if (lane !== 'needs') rail.append(h('i', { class: `head head-${lane}` }));
      const action = lane === 'running' ? h('a', { class: 'button small', href, text: 'Open' })
        : h('a', { class: 'button small needs', href: '#/attention', text: lane === 'ready' ? 'Review' : 'Decide' });
      return h('div', { class: `board-row board-${lane}`, 'data-session': s ? s.id : d.session, 'data-lane': lane },
        h('span', { class: 'board-name' }, dot(lane), h('span', { class: 'board-id' }, h('a', { href, text: name }), h('span', { class: 'board-meta', text: meta }))),
        rail,
        h('span', { class: 'board-cost' }),
        h('span', { class: 'board-act' }, action));
    };
    fill(board, boardAxis(scale, 'Session'), rows.map(row));
  }

  // A workspace is reserved at once and set up by a queued operation; it
  // opens when that operation has succeeded.
  async function createWorkspace(project, field, button, status) {
    const title = field.value.trim();
    if (!title) { field.focus(); return; }
    button.disabled = true;
    button.textContent = 'Creating…';
    status.textContent = 'Setting up the workspace…';
    const reset = (text) => {
      button.disabled = false;
      button.textContent = 'Create workspace';
      status.textContent = text;
    };
    let op;
    try {
      op = await api('POST', '/v1/workspaces', { project_id: project.id, title });
    } catch (e) {
      reset(`Could not create the workspace: ${e.message}`);
      return;
    }
    const done = await awaitOperation(op.id);
    if (!done || done.state !== 'succeeded') {
      reset(done ? `The workspace did not start: ${done.error ? done.error.message : done.state}.` : 'The workspace is still being set up; it appears in the list when it is ready.');
      if (state.workspaces.project === project.id) loadWorkspaces(project.id);
      return;
    }
    const id = done.workspace_id || op.workspace_id;
    field.value = '';
    toast(`Workspace ${title} is ready.`);
    if (state.workspaces.project === project.id) loadWorkspaces(project.id);
    if (status.isConnected) location.hash = `#/w/${enc(id)}`;
  }

  // ---------------------------------------------------------------
  // One branch: the status strip, the conversation, the message box, and
  // the inspector

  function describe(activity) {
    const [kind] = Object.keys(activity);
    const v = activity[kind];
    switch (kind) {
      case 'harness':
        switch (v.type) {
          case 'tool_started': return `tool ${v.name}`;
          case 'message_delta': return v.text;
          case 'permission_requested': return `asks to use ${v.request.tool}`;
          case 'turn_ended': return `turn ${v.turn} ended`;
          case 'turn_accepted': return `turn ${v.turn} started`;
          case 'warning': return `warning: ${v.message}`;
          case 'steer_accepted': return 'steering delivered';
          case 'steer_rejected': return `steering refused: ${v.reason}`;
          default: return v.type.replace(/_/g, ' ');
        }
      case 'prompt': return `prompt: ${v}`;
      case 'decision': return `${v.allowed ? 'allowed' : 'denied'} ${v.tool}${v.message ? `: ${v.message}` : ''}`;
      case 'snapshot': return `candidate ${v.commit.slice(0, 10)} (${v.files_changed} files +${v.insertions} −${v.deletions})`;
      case 'status': return `status: ${statusOf({ status: v }).text}`;
      case 'warning': return `warning: ${v}`;
      case 'checkpoint': return `checkpoint ${v.turn} at ${v.commit.slice(0, 10)}`;
      case 'turn_cost': return `turn cost ${money(v.cost_usd)}`;
      case 'steered': return `steered by ${v.by}: ${v.text}`;
      case 'stalled': return 'stalled: no harness activity';
      case 'resumed': return 'active again';
      case 'message': return `${v.kind} from ${v.from} to ${v.to}: ${v.text}`;
      case 'recovered': return `recovered: ${v.reason}`;
      default: return kind.replace(/_/g, ' ');
    }
  }

  function eventItem(e) {
    return h('li', {}, h('time', { datetime: new Date(e.at_ms).toISOString(), text: clock(e.at_ms) }), describe(e.activity));
  }

  // Message deltas arrive a few words at a time: join each run of them.
  function coalesce(events) {
    const out = [];
    for (const e of events) {
      const prev = out[out.length - 1];
      const delta = e.activity.harness && e.activity.harness.type === 'message_delta';
      const prevDelta = prev && prev.activity.harness && prev.activity.harness.type === 'message_delta';
      if (delta && prevDelta) {
        prev.activity = { harness: { type: 'message_delta', text: prev.activity.harness.text + e.activity.harness.text } };
      } else {
        out.push({ at_ms: e.at_ms, activity: e.activity });
      }
    }
    return out.map((e) => {
      if (e.activity.harness && e.activity.harness.type === 'message_delta') {
        const text = e.activity.harness.text.trim();
        return { at_ms: e.at_ms, activity: { harness: { type: 'message_delta', text: `said: ${text.length > 400 ? `${text.slice(0, 399)}…` : text}` } } };
      }
      return e;
    });
  }

  // The event page as a conversation. A prompt or a steer opens a turn,
  // which gathers what the agent said (one paragraph per run of deltas
  // between tool calls), the tools it called, and how it ended. Events
  // before the window's first prompt open a turn with no prompt; statuses
  // and session bookkeeping belong to Activity alone.
  function conversation(events) {
    const turns = [];
    let turn = null;
    let cut = true;
    const open = (text, steer, at) => {
      turn = {
        text, steer, said: [], tools: [], parts: [], pending: [], status: null, checkpoint: null, candidate: null,
        cost: null, started_ms: at || null, ended_ms: null,
      };
      turns.push(turn);
      cut = true;
      return turn;
    };
    for (const { activity: a, at_ms: at } of events) {
      const ev = a.harness || {};
      if (a.prompt != null) open(a.prompt, false, at);
      else if (a.steered) open(a.steered.text, true, at);
      else if (ev.type === 'message_delta') {
        const t = turn || open(null, false, at);
        if (cut) { t.said.push(ev.text); t.parts.push({ tools: t.pending, text: ev.text }); t.pending = []; }
        else { t.said[t.said.length - 1] += ev.text; t.parts[t.parts.length - 1].text += ev.text; }
        cut = false;
      } else if (ev.type === 'tool_started') {
        const t = turn || open(null, false, at);
        t.tools.push(ev.name);
        t.pending.push(ev.name);
        cut = true;
      } else if (a.snapshot) (turn || open(null, false, at)).candidate = a.snapshot;
      else if (a.checkpoint) (turn || open(null, false, at)).checkpoint = a.checkpoint;
      else if (a.turn_cost) (turn || open(null, false, at)).cost = a.turn_cost.cost_usd;
      else if (a.status && turn && !['running', 'waiting'].includes(a.status.state)) { turn.status = a.status; turn.ended_ms = at || null; }
    }
    for (const t of turns) {
      // Calls after the last paragraph are notes with nothing beside them.
      if (t.pending.length) t.parts.push({ tools: t.pending, text: '' });
      t.pending = [];
      t.said = t.said.map((s) => s.trim()).filter(Boolean);
      for (const part of t.parts) part.text = part.text.trim();
    }
    return turns;
  }

  // How many of a turn's calls read, edited and ran, by the verb each
  // name opens with; the rest are counted as "other".
  function toolCounts(names) {
    const counts = { read: 0, edit: 0, run: 0, other: 0 };
    for (const name of names) {
      const kind = toolIcon(name);
      counts[kind === 'eye' ? 'read' : kind === 'pencil' ? 'edit' : kind === 'terminal' ? 'run' : 'other'] += 1;
    }
    return counts;
  }
  const countsText = (counts) => Object.entries(counts).filter(([, n]) => n).map(([k, n]) => `${k} ${n}`).join(', ');
  function spanText(ms) {
    if (ms == null || ms < 0) return null;
    const s = Math.round(ms / 1000);
    if (s < 60) return `${s}s`;
    const m = Math.floor(s / 60);
    return m < 60 ? `${m}m ${String(s % 60).padStart(2, '0')}s` : `${Math.floor(m / 60)}h ${m % 60}m`;
  }

  // ---------------------------------------------------------------
  // The agent's text as a small markdown, built as elements and never
  // parsed as HTML: paragraphs, headings, lists, quotes, rules, pipe
  // tables, inline code, emphasis, and links to http(s) addresses only.
  // A fenced block is code under a header with a copy button; one that
  // holds JSON is shown as the facts it holds, its text one click away.

  const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([\w.+-]*)\s*$/;
  const LIST = /^(\s*)([-*+]|\d{1,3}[.)])\s+(.*)$/;
  const RULE = /^ {0,3}([-*_])(\s*\1){2,}\s*$/;
  const HEADING = /^ {0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
  const QUOTE = /^ {0,3}>/;
  const TABLE_SEP = /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/;
  const INLINE = /(`+)([^`][\s\S]*?)\1(?!`)|\*\*([^*]+?)\*\*|(?<![\w*])\*([^*\s][^*]*?)\*(?![\w*])|(?<![\w])_([^_\s][^_]*?)_(?![\w])|~~([^~]+?)~~|\[([^\]\n]+)\]\(([^)\s]+)\)/g;

  function rich(text) {
    const out = [];
    const lines = text.split('\n');
    let para = [];
    const flush = () => {
      if (para.length) out.push(h('p', {}, inline(para.join('\n'))));
      para = [];
    };
    let i = 0;
    while (i < lines.length) {
      const line = lines[i];
      const fence = FENCE.exec(line);
      if (fence) {
        flush();
        const close = new RegExp(`^ {0,3}\\${fence[1][0]}{${fence[1].length},}\\s*$`);
        const body = [];
        i += 1;
        while (i < lines.length && !close.test(lines[i])) body.push(lines[i++]);
        i += 1;
        out.push(codeBlock(body.join('\n'), fence[2].toLowerCase()));
        continue;
      }
      if (!line.trim()) { flush(); i += 1; continue; }
      const heading = HEADING.exec(line);
      if (heading) {
        flush();
        out.push(h(heading[1].length <= 2 ? 'h3' : 'h4', { class: 'md-h' }, inline(heading[2])));
        i += 1;
        continue;
      }
      if (RULE.test(line)) { flush(); out.push(h('hr')); i += 1; continue; }
      if (QUOTE.test(line)) {
        flush();
        const quoted = [];
        while (i < lines.length && QUOTE.test(lines[i])) quoted.push(lines[i++].replace(/^ {0,3}> ?/, ''));
        out.push(h('blockquote', {}, rich(quoted.join('\n'))));
        continue;
      }
      if (LIST.test(line)) {
        flush();
        const [el, next] = list(lines, i);
        out.push(el);
        i = next;
        continue;
      }
      if (line.includes('|') && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1])) {
        flush();
        const rows = [];
        while (i < lines.length && lines[i].includes('|') && lines[i].trim()) rows.push(lines[i++]);
        out.push(table(rows));
        continue;
      }
      para.push(line);
      i += 1;
    }
    flush();
    return out;
  }

  // A list from line `i`: items at the first item's indent; a deeper item
  // nests under the one before it; a shallower one ends this list. An
  // indented line that is not an item continues the last item; one at the
  // margin ends the list, as agents mean it to. Returns the element and
  // the line after the list.
  function list(lines, i) {
    const first = LIST.exec(lines[i]);
    const base = first[1].length;
    const ordered = /\d/.test(first[2]);
    const start = ordered ? parseInt(first[2], 10) : 1;
    const el = h(ordered ? 'ol' : 'ul', { start: start !== 1 ? String(start) : null });
    let item = null;
    while (i < lines.length) {
      const line = lines[i];
      if (!line.trim()) {
        const next = lines[i + 1];
        const continues = next != null && (LIST.test(next) ? LIST.exec(next)[1].length >= base : /^\s{2,}\S/.test(next));
        if (!continues) break;
        i += 1;
        continue;
      }
      const m = LIST.exec(line);
      if (m && m[1].length < base) break;
      if (m && m[1].length === base) {
        item = h('li', {}, inline(m[3]));
        el.append(item);
        i += 1;
      } else if (m && item) {
        const [sub, next] = list(lines, i);
        item.append(sub);
        i = next;
      } else if (item && /^\s/.test(line)) {
        item.append(' ', ...inline(line.trim()));
        i += 1;
      } else break;
    }
    return [el, i];
  }

  const cells = (row) => {
    let t = row.trim();
    if (t.startsWith('|')) t = t.slice(1);
    if (t.endsWith('|')) t = t.slice(0, -1);
    return t.split(/(?<!\\)\|/).map((c) => c.trim().replace(/\\\|/g, '|'));
  };
  function table(rows) {
    const head = cells(rows[0]);
    const numeric = cells(rows[1]).map((c) => c.endsWith(':') && !c.startsWith(':'));
    const tr = (cols, tag) => h('tr', {}, head.map((_, k) => h(tag, { class: numeric[k] ? 'num' : null }, inline(cols[k] || ''))));
    return h('div', { class: 'md-table-wrap' }, h('table', { class: 'md-table' },
      h('thead', {}, tr(head, 'th')), h('tbody', {}, rows.slice(2).map((r) => tr(cells(r), 'td')))));
  }

  const breaks = (text) => text.split('\n').flatMap((part, k) => (k ? [h('br'), part] : [part]));
  function inline(text) {
    const out = [];
    let last = 0;
    for (const m of text.matchAll(INLINE)) {
      if (m.index > last) out.push(...breaks(text.slice(last, m.index)));
      if (m[2] != null) out.push(h('code', { text: m[2].replace(/^ (?=\S)| (?<=\S)$/g, '') }));
      else if (m[3] != null) out.push(h('strong', {}, inline(m[3])));
      else if (m[4] != null || m[5] != null) out.push(h('em', {}, inline(m[4] != null ? m[4] : m[5])));
      else if (m[6] != null) out.push(h('s', {}, inline(m[6])));
      else out.push(link(m[7], m[8]));
      last = m.index + m[0].length;
    }
    if (last < text.length) out.push(...breaks(text.slice(last)));
    return out;
  }
  // Only an absolute web address becomes a link, opened in a new tab; any
  // other target stays as the text it was written as.
  function link(label, href) {
    let url = null;
    if (/^https?:/i.test(href)) { try { url = new URL(href); } catch (_) { url = null; } }
    if (!url) return [`[`, ...inline(label), `](${href})`];
    return h('a', { href: url.href, target: '_blank', rel: 'noopener noreferrer' }, inline(label));
  }

  function codeBlock(text, lang) {
    const parsed = lang === 'json' || lang === '' ? parseJson(text) : undefined;
    if (parsed !== undefined) return jsonBlock(parsed);
    return h('figure', { class: 'code-block' }, codeHead(lang || 'Code', text), h('pre', {}, h('code', { text })));
  }
  function parseJson(text) {
    const t = text.trim();
    if (!/^[[{]/.test(t)) return undefined;
    try { return JSON.parse(t); } catch (_) { return undefined; }
  }
  function codeHead(label, text) {
    const copy = h('button', { type: 'button', class: 'copy', 'aria-label': `Copy ${label}` }, icon('copy', true), 'Copy');
    copy.addEventListener('click', async () => {
      try { await navigator.clipboard.writeText(text); } catch (_) { return; }
      copy.dataset.done = '';
      copy.lastChild.textContent = 'Copied';
      setTimeout(() => { delete copy.dataset.done; copy.lastChild.textContent = 'Copy'; }, 1500);
    });
    return h('div', { class: 'code-head' }, h('span', { class: 'code-lang', text: label }), h('span', { class: 'spacer' }), copy);
  }
  // JSON: a flat object is a list of its keys and values; anything else is
  // its text, pretty-printed, with keys, strings and numbers told apart.
  const scalar = (v) => v === null || ['string', 'number', 'boolean'].includes(typeof v);
  const jsonClass = (v) => (v === null ? 'j-null' : typeof v === 'boolean' ? 'j-bool' : typeof v === 'number' ? 'j-num' : 'j-str');
  const JSON_TOKEN = /("(?:[^"\\]|\\.)*")(\s*:)?|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|\b(true|false)\b|\b(null)\b|([{}[\],:])/g;
  function jsonTokens(text) {
    const out = [];
    let last = 0;
    for (const m of text.matchAll(JSON_TOKEN)) {
      if (m.index > last) out.push(text.slice(last, m.index));
      if (m[1] != null) {
        out.push(h('span', { class: m[2] ? 'j-key' : 'j-str', text: m[1] }));
        if (m[2]) out.push(h('span', { class: 'j-punct', text: m[2] }));
      } else if (m[3] != null) out.push(h('span', { class: 'j-num', text: m[3] }));
      else if (m[4] != null) out.push(h('span', { class: 'j-bool', text: m[4] }));
      else if (m[5] != null) out.push(h('span', { class: 'j-null', text: m[5] }));
      else out.push(h('span', { class: 'j-punct', text: m[6] }));
      last = m.index + m[0].length;
    }
    if (last < text.length) out.push(text.slice(last));
    return out;
  }
  function jsonBlock(value) {
    const pretty = JSON.stringify(value, null, 2);
    const keys = value && typeof value === 'object' && !Array.isArray(value) ? Object.keys(value) : [];
    const flat = keys.length > 0 && keys.length <= 24 && keys.every((k) => scalar(value[k]));
    const pre = h('pre', {}, h('code', {}, jsonTokens(pretty)));
    if (!flat) return h('figure', { class: 'code-block is-json' }, codeHead('JSON', pretty), pre);
    const facts = h('dl', { class: 'json-facts' }, keys.map((k) => [h('dt', { text: k }),
      h('dd', {}, typeof value[k] === 'string' ? value[k] : h('span', { class: jsonClass(value[k]), text: JSON.stringify(value[k]) }))]));
    return h('figure', { class: 'code-block is-json' }, codeHead('JSON', pretty), facts,
      h('details', {}, h('summary', { text: 'Show as text' }), pre));
  }

  // What the agent said, under a small head that names it as the agent's
  // and which harness it was.
  function agentBlock(said, harness) {
    return h('div', { class: 'agent' },
      h('div', { class: 'agent-head' },
        h('img', { class: 'agent-mark', src: 'icon.svg', alt: '', width: '18', height: '18' }),
        h('span', { class: 'agent-name', text: harness || 'Agent' }),
        h('span', { class: 'ai-tag', text: 'AI', title: 'Written by the agent' })),
      said.map((s) => h('div', { class: 'rich' }, rich(s))));
  }

  // Five blocks that show how much of a file's change adds and removes.
  function fileBar(add, del) {
    const total = add + del;
    let adds = total ? Math.round((add / total) * 5) : 0;
    if (add && del) adds = Math.min(4, Math.max(1, adds));
    const dels = total ? 5 - adds : 0;
    return h('span', { class: 'file-bar', 'aria-hidden': 'true' },
      Array.from({ length: 5 }, (_, k) => h('i', { class: k < adds ? 'add' : k < adds + dels ? 'del' : null })));
  }

  // A unified diff split at each file: its path, its counts, and its lines
  // marked as they render. The file list already names the path, so git's
  // own header lines are dropped; a mode, rename or binary note stays.
  const GIT_HEADER = /^(diff --git |index |--- |\+\+\+ )/;
  function parseDiff(text) {
    const files = [];
    let file = null;
    let inHunk = false;
    for (const line of text.split('\n')) {
      if (line.startsWith('diff --git ')) {
        const m = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
        file = { path: m ? m[2] : line.slice(11), add: 0, del: 0, lines: [] };
        files.push(file);
        inHunk = false;
      }
      if (!file) continue;
      let kind = null;
      if (line.startsWith('@@')) { kind = 'hunk'; inHunk = true; }
      else if (!inHunk) {
        kind = 'file';
        if (line.startsWith('+++ b/')) file.path = line.slice(6);
        if (GIT_HEADER.test(line)) continue;
      } else if (line.startsWith('+')) { kind = 'add'; file.add += 1; }
      else if (line.startsWith('-')) { kind = 'del'; file.del += 1; }
      file.lines.push({ kind, text: line });
    }
    return files;
  }

  function policy(allowAll) {
    return allowAll ? { mode: 'allow' } : undefined;
  }

  // What a branch's buttons do. `started` is said once the server took the
  // request; `finished`, when its operation succeeded.
  const ACTIONS = {
    merge: {
      label: 'Merge', busy: 'Merging…', scope: 'merge', rule: when.merge,
      confirm: (b) => [`Merge ${b}?`, `Merge ${b} into the checked-out branch? Its check runs on the merge result first, and nothing changes unless it passes.`],
      run: (b) => api('POST', `${branchPath(b)}/merge`, {}),
      started: 'Merging. Its check runs on the merge result first.',
      finished: (info) => (info && info.status.state === 'merged' ? `Merged into ${info.status.target}.` : 'Merged.'),
    },
    resume: {
      label: 'Resume', busy: 'Resuming…', scope: 'run', rule: when.resume,
      run: (b) => api('POST', `${branchPath(b)}/send`, { prompt: RESUME_PROMPT }),
      started: 'Resuming. A new turn is starting.',
    },
    cancel: {
      label: 'Cancel turn', busy: 'Cancelling…', scope: 'run', rule: when.cancel, danger: true,
      confirm: (b) => ['Cancel turn?', `Cancel ${b}'s running turn, and those of every branch it delegated to?`],
      run: (b) => api('POST', `${branchPath(b)}/cancel`, {}),
      started: 'Cancelling the running turn.',
      finished: () => 'Turn cancelled.',
    },
  };

  // The strip's one main action for a state; a state without one has none.
  const PRIMARY = {
    ready: ACTIONS.merge,
    interrupted: ACTIONS.resume,
  };

  // Rare actions, in the ⋯ menu. Fork puts the message box in fork mode.
  const MENU = [
    ACTIONS.cancel,
    { label: 'Fork…', scope: 'run', rule: when.fork, pick: () => { view.forking = true; updateBranch(); $('composer-text').focus(); } },
  ];

  // The message box's verb, by the same rules: a running branch steers,
  // any other sends a follow-up (refused, with the reason in the box, while
  // it waits), and Fork from the menu holds until sent or cancelled.
  const COMPOSER = {
    send: {
      label: 'Send', title: 'Send a follow-up prompt', scope: 'run', rule: when.send, allowAll: true,
      placeholder: (b) => `Follow up on ${b}…`,
      run: (b, text, all) => api('POST', `${branchPath(b)}/send`, { prompt: text, policy: policy(all) }),
      started: 'Sent. A new turn is starting.',
    },
    steer: {
      label: 'Steer', title: 'Steer the running turn', scope: 'run', rule: when.steer, allowAll: false,
      placeholder: (b) => `Steer ${b}'s running turn…`,
      run: (b, text) => api('POST', `${branchPath(b)}/steer`, { text }),
      started: 'Steering sent. The running turn reads it next.',
    },
    fork: {
      label: 'Fork', title: 'Fork with a prompt', scope: 'run', rule: when.fork, allowAll: true, opens: true,
      placeholder: (b) => `What should a fork of ${b} do?`,
      run: (b, text, all) => api('POST', `${branchPath(b)}/fork`, { prompt: text, policy: policy(all) }),
      started: 'Forked. The new branch is starting.',
    },
  };

  const lacks = (action) => (can(action.scope) ? null : `this token lacks the ${action.scope} scope`);
  const applies = (action, info) => !lacks(action) && !action.rule(info);

  function composerMode(info, forking) {
    const mode = forking ? COMPOSER.fork : when.steer(info) ? COMPOSER.send : COMPOSER.steer;
    return { mode, why: lacks(mode) || mode.rule(info) };
  }

  // Run one of a branch's actions and say what became of it, in words.
  async function act(action, name, ...args) {
    let result;
    try {
      result = await action.run(name, ...args);
    } catch (e) {
      toast(`Could not ${action.label.toLowerCase()}: ${e.message}`);
      return null;
    }
    const opened = action.opens && result && result.branches && result.branches[0];
    toast(action.started, opened ? branchHref(opened) : null);
    if (result && result.id && result.state) followOperation(result.id, action, name);
    refreshBranch(name);
    if (view && view.name === name) loadOperations(view);
    return result || {};
  }

  async function followOperation(id, action, name) {
    const repo = state.repo;
    const op = await awaitOperation(id);
    if (!op) return;
    let info = null;
    if (name && repo === state.repo) {
      try { info = await api('GET', branchPath(name)); state.branches.set(name, info); } catch (_) { /* the list refresh catches up */ }
    }
    if (state.pending.get(name) === action.busy) state.pending.delete(name);
    if (op.state !== 'succeeded') toast(`${action.label} ${op.state}${op.error ? `: ${op.error.message}` : '.'}`);
    else if (action.finished) {
      if (action === ACTIONS.merge) state.merged.add(name);
      toast(action.finished(info));
    }
    branchesChanged();
  }

  async function runAction(action, name) {
    if (action.confirm) {
      const [title, text] = action.confirm(name);
      if (!(await confirmDialog(title, text, action.label, action.danger))) return;
    }
    state.pending.set(name, action.busy);
    updateBranch();
    const result = await act(action, name);
    if (!result || !result.id) state.pending.delete(name);
    updateBranch();
  }

  // The open branch: what its panes show, kept across live updates.
  let view = null;

  async function renderBranch() {
    const name = state.route.branch;
    let info = state.branches.get(name);
    if (!info) {
      try { info = await api('GET', branchPath(name)); state.branches.set(name, info); } catch (e) {
        main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
          h('h1', { text: name }), h('p', { class: 'error', text: e.message })));
        return;
      }
    }
    const v = {
      name, events: [], first: 0, next: null, chain: null, queued: false,
      forking: false, tab: 'changes', file: null, files: null, diffFor: undefined, truncated: 0,
      open: new Set(), signature: null, operations: null, groundFor: null,
    };
    view = v;
    main(h('div', { class: 'branch' },
      h('section', { class: 'focus', 'aria-label': 'Conversation' },
        h('header', { class: 'strip', id: 'strip' }),
        h('div', { class: 'convo', id: 'convo' }, skeleton()),
        composer(() => submitComposer())),
      h('aside', { class: 'inspector', id: 'inspector', 'aria-label': 'Inspector' },
        tablist(BRANCH_TABS, () => v.tab, (k) => selectTab(v, k)),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-changes', 'aria-labelledby': 'tab-changes' }, h('div', { id: 'changes' })),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-ground', 'aria-labelledby': 'tab-ground', hidden: true },
          h('div', { id: 'ground' }, h('p', { class: 'muted', text: 'Loading…' }))),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-details', 'aria-labelledby': 'tab-details', hidden: true },
          h('dl', { class: 'facts', id: 'facts' }),
          h('h2', { text: 'Checkpoints' }), h('div', { id: 'checkpoints' }, h('p', { class: 'muted', text: 'Loading…' })),
          h('h2', { text: 'Operations' }), h('div', { id: 'operations' }, h('p', { class: 'muted', text: 'Loading…' }))),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-activity', 'aria-labelledby': 'tab-activity', hidden: true },
          h('ul', { class: 'events', id: 'events', 'aria-label': 'Recent events' }, h('li', { class: 'muted', text: 'Loading…' }))))));
    updateBranch();
    v.chain = pull(v).then(() => drawTimeline(v), (e) => {
      if (view !== v) return;
      fill($('convo'), h('p', { class: 'error', text: e.message }));
      fill($('events'), h('li', { class: 'error', text: e.message }));
    });
    loadOperations(v);
    loadGround().then(() => drawGround(v));
  }

  const BRANCH_TABS = [['changes', 'Changes', 'changes-count'], ['ground', 'Ground', 'ground-count'], ['details', 'Details'], ['activity', 'Activity', 'activity-count']];

  // The inspector's tabs: [key, label, id of a count beside it]. Arrow keys
  // move between them.
  function tablist(tabs, current, pick) {
    return h('div', { class: 'tabs', role: 'tablist', 'aria-label': 'Inspector', onkeydown: (ev) => {
      const at = tabs.findIndex(([k]) => k === current());
      const by = { ArrowRight: 1, ArrowLeft: -1 }[ev.key];
      if (!by) return;
      const [k] = tabs[(at + by + tabs.length) % tabs.length];
      pick(k);
      $(`tab-${k}`).focus();
    } }, tabs.map(([k, label, count]) => h('button', {
      type: 'button', role: 'tab', id: `tab-${k}`, 'aria-controls': `panel-${k}`,
      'aria-selected': String(k === current()), tabindex: k === current() ? '0' : '-1',
      onclick: () => pick(k),
    }, label, count ? [' ', h('span', { class: 'tab-count', id: count })] : null)));
  }

  function showTab(tabs, tab) {
    for (const [k] of tabs) {
      const on = k === tab;
      $(`tab-${k}`).setAttribute('aria-selected', String(on));
      $(`tab-${k}`).tabIndex = on ? 0 : -1;
      $(`panel-${k}`).hidden = !on;
    }
  }

  function selectTab(v, tab) {
    v.tab = tab;
    showTab(BRANCH_TABS, tab);
    drawTimeline(v);
  }

  // The latest window of events first, then everything after it, page by
  // page; a burst of stream events for this branch asks for one more pull.
  async function pull(v) {
    for (;;) {
      const query = v.next == null ? '' : `cursor=${v.next}&`;
      const page = await api('GET', `${branchPath(v.name)}/event-page?${query}limit=${EVENT_WINDOW}`);
      if (v.next == null) v.first = page.next_cursor - page.events.length;
      v.events.push(...page.events);
      v.next = page.next_cursor;
      if (v.events.length > EVENT_KEEP) {
        v.first += v.events.length - EVENT_KEEP;
        v.events.splice(0, v.events.length - EVENT_KEEP);
      }
      if (page.events.length < EVENT_WINDOW || v.next >= page.total) return;
    }
  }

  function refreshTimeline(v) {
    if (v.queued || !v.chain) return;
    v.queued = true;
    v.chain = v.chain
      .then(() => new Promise((r) => setTimeout(r, 150)))
      .then(() => { v.queued = false; return pull(v); })
      .then(() => drawTimeline(v))
      .catch(() => { v.queued = false; });
  }

  function drawTimeline(v) {
    if (view !== v || v.next == null) return;
    const box = $('convo');
    const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 48;
    const turns = conversation(v.events);
    // A turn's number counts the prompts before it, not the steers.
    v.turnsBefore = (i) => turns.slice(0, i).filter((t) => !t.steer && t.text != null).length;
    const info = state.branches.get(v.name);
    const working = info && ['running', 'waiting'].includes(info.status.state);
    fill(box,
      v.first > 0 ? h('p', { class: 'convo-note', text: `Earlier turns are not shown here: this is the latest of ${plural(v.first + v.events.length, 'event')}.` }) : null,
      turns.length ? turns.map((t, i) => turnItem(t, i, v, working && i === turns.length - 1))
        : working ? null : h('section', { class: 'empty convo-empty' },
          h('span', { class: 'empty-mark', 'aria-hidden': 'true' }, icon('terminal')),
          h('h2', { text: 'No turns yet' }),
          h('p', { text: 'A turn starts when the agent gets a prompt. What it says reads here as a report, its tool calls in the margin beside the paragraph they led to, and each turn ends with a receipt. Write the first prompt below.' })),
      working && !turns.length ? workingLine(info) : null);
    if (atEnd || !v.scrolled) box.scrollTop = box.scrollHeight;
    v.scrolled = true;
    const calls = v.events.filter((e) => e.activity.harness && e.activity.harness.type === 'tool_started').length;
    if ($('activity-count')) $('activity-count').textContent = calls ? String(calls) : '';
    if (v.tab === 'activity') {
      const events = coalesce(v.events);
      fill($('events'), ...(events.length ? events.slice().reverse().map(eventItem) : [h('li', { class: 'muted', text: 'No events yet.' })]));
    }
    if (v.tab === 'details') {
      const cps = v.events.filter((e) => e.activity.checkpoint).map((e) => e.activity.checkpoint);
      fill($('checkpoints'), cps.length
        ? h('ul', { class: 'events' }, cps.reverse().map((cp) => h('li', {}, `turn ${cp.turn} · `, h('span', { class: 'mono', text: cp.commit.slice(0, 12) }), ` · ${plural(cp.files_changed, 'file')} `, counts(cp.insertions, cp.deletions))))
        : h('p', { class: 'muted', text: v.first > 0 ? 'None in the latest events.' : 'None yet: each finished turn leaves one.' }));
    }
  }

  function workingLine(info) {
    return h('p', { class: 'outcome' }, dot('running'), info.status.state === 'waiting' ? 'Waiting for its prerequisites…' : 'Working…');
  }

  // A tool call as a note in the margin: the name the harness gave it, with
  // the icon for its verb. Clicking opens the Activity tab, where the call
  // is one of the events.
  function note(name, v) {
    return h('button', {
      type: 'button', class: 'note', title: name,
      onclick: () => { if (v) selectTab(v, 'activity'); },
    }, icon(toolIcon(name), true), h('span', { text: name }));
  }

  // The turn's receipt: how long it took, what it cost (the engine's own
  // record of the turn's spend, when something measured it), what its
  // calls did, what it changed, and how it ended.
  function receipt(t) {
    const c = t.checkpoint || t.candidate;
    const lane = t.status ? laneOf({ name: '', status: t.status }, new Map()).key : null;
    if (!t.status && !c) return null;
    const took = t.started_ms && t.ended_ms && t.ended_ms - t.started_ms >= 1000 ? spanText(t.ended_ms - t.started_ms) : null;
    const did = t.tools.length ? countsText(toolCounts(t.tools)) : null;
    const figure = (label, ...value) => h('span', { class: 'figure' }, h('span', { class: 'figure-label', text: label }), h('span', { class: 'figure-value' }, value));
    return h('div', { class: `outcome receipt outcome-${lane || 'done'}` },
      h('span', { class: 'receipt-state' },
        lane === 'ready' ? h('span', { class: 'tick', 'aria-hidden': 'true', text: '✓' }) : lane ? dot(lane) : null,
        t.status ? cap(statusOf({ status: t.status }).text) : 'Checkpoint'),
      h('span', { class: 'receipt-figures' },
        took ? figure('took', took) : null,
        did ? figure('calls', did) : null,
        c ? figure('commit', h('span', { class: 'mono', text: c.commit.slice(0, 7) })) : null,
        c ? figure('changed', `${plural(c.files_changed, 'file')} `, counts(c.insertions, c.deletions)) : null,
        t.cost != null ? figure('cost', money(t.cost)) : null));
  }

  function turnItem(t, i, v, last) {
    const info = state.branches.get(v.name);
    const number = v.first > 0 ? null : 1 + v.turnsBefore(i);
    const outcome = receipt(t);
    return h('article', { class: 'turn' },
      t.text != null ? h('div', { class: 'turn-row turn-ask' },
        h('span', { class: 'turn-when' }, h('b', { text: t.steer ? 'Steered' : number ? `Turn ${number}` : 'Turn' }), t.started_ms ? ` · ${clock(t.started_ms).slice(0, 5)}` : null),
        h('div', { class: t.steer ? 'you steer' : 'you' },
          t.steer ? h('span', { class: 'you-label', text: 'Steered' }) : null, h('p', { text: t.text }))) : null,
      t.parts.length ? reportBlock(t.parts, info && info.harness, v) : null,
      outcome,
      last && !outcome && info ? workingLine(info) : null);
  }

  // What the agent said as a report: each paragraph beside the calls that
  // came before it, in the margin.
  function reportBlock(parts, harness, v) {
    return h('div', { class: 'agent report' },
      h('div', { class: 'agent-head' },
        h('img', { class: 'agent-mark', src: 'icon.svg', alt: '', width: '18', height: '18' }),
        h('span', { class: 'agent-name', text: harness || 'Agent' }),
        h('span', { class: 'ai-tag', text: 'AI', title: 'Written by the agent' })),
      parts.map((part) => h('div', { class: 'turn-row' },
        h('aside', { class: 'notes', 'aria-label': part.tools.length ? plural(part.tools.length, 'tool call') : null }, part.tools.map((name) => note(name, v))),
        part.text ? h('div', { class: 'rich' }, rich(part.text)) : h('div', { class: 'rich rich-empty' }))));
  }

  function composer(submit) {
    const area = h('textarea', { id: 'composer-text', rows: '2' });
    area.addEventListener('keydown', (ev) => {
      if (ev.key === 'Enter' && (ev.metaKey || ev.ctrlKey)) { ev.preventDefault(); submit(); }
    });
    return h('div', { class: 'composer', id: 'composer' },
      h('label', { class: 'visually-hidden', for: 'composer-text', id: 'composer-label' }),
      area,
      h('div', { class: 'composer-tools' },
        h('label', { class: 'toggle', id: 'composer-allow-box', title: 'Like --yes. Otherwise every tool request is denied.' },
          h('input', { type: 'checkbox', id: 'composer-allow' }), h('span', { text: 'Allow every tool' })),
        h('button', { type: 'button', class: 'ghost', id: 'composer-cancel', text: 'Cancel fork', onclick: () => {
          view.forking = false;
          updateBranch();
          $('composer-text').focus();
        } }),
        h('span', { class: 'spacer' }),
        h('span', { class: 'kbd-hint', 'aria-hidden': 'true' }, h('kbd', { text: MOD_KEY }), h('kbd', { text: '↵' })),
        h('button', { type: 'button', class: 'primary', id: 'composer-go', onclick: () => submit() })));
  }
  // The modifier that sends with Enter, as the platform names it.
  const MOD_KEY = /Mac|iPhone|iPad/.test(navigator.platform || '') ? '⌘' : 'Ctrl';

  function applyComposer(info) {
    const { mode, why } = composerMode(info, view.forking);
    const area = $('composer-text');
    $('composer').dataset.mode = Object.keys(COMPOSER).find((k) => COMPOSER[k] === mode);
    $('composer-label').textContent = mode.title;
    area.placeholder = why ? sentence(why, info.name) : mode.placeholder(info.name);
    area.disabled = !!why;
    $('composer-go').disabled = !!why;
    $('composer-go').textContent = mode.label;
    $('composer-allow-box').hidden = !mode.allowAll;
    $('composer-cancel').hidden = mode !== COMPOSER.fork;
  }

  async function submitComposer() {
    const v = view;
    const info = state.branches.get(v.name);
    if (!info) return;
    const { mode, why } = composerMode(info, v.forking);
    const area = $('composer-text');
    const allow = $('composer-allow');
    const go = $('composer-go');
    if (why || go.disabled) return;
    const text = area.value.trim();
    if (!text) { area.focus(); return; }
    go.disabled = true;
    const result = await act(mode, v.name, text, allow.checked);
    if (view !== v) return;
    if (result) {
      area.value = '';
      allow.checked = false;
      if (mode === COMPOSER.fork) v.forking = false;
    }
    applyComposer(state.branches.get(v.name) || info);
  }

  function strip(info) {
    const s = statusOf(info);
    const lane = laneOf(info, attention()).key;
    const primary = PRIMARY[info.status.state];
    const busy = state.pending.get(info.name);
    const menu = MENU.filter((a) => applies(a, info));
    const meta = [info.harness, plural(info.turns, 'turn'), info.cost_usd != null ? money(info.cost_usd) : null].filter(Boolean).join(' · ');
    let action = null;
    if (busy) action = h('button', { type: 'button', class: 'primary', disabled: true, text: busy });
    else if (primary && applies(primary, info)) {
      action = h('button', { type: 'button', class: 'primary', text: primary.label, onclick: () => runAction(primary, info.name) });
    }
    return [
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('div', { class: 'strip-title' }, h('h1', { tabindex: '-1', text: info.name }), h('p', { class: 'strip-meta', text: meta })),
      s.key === 'merged'
        ? h('p', { class: 'state state-merged' }, checkMark(state.merged.has(info.name)), cap(s.text))
        : h('p', { class: `state state-${lane}` }, dot(lane), cap(s.text)),
      action || menu.length ? h('div', { class: 'strip-actions' }, action, menu.length ? menuOf(menu, info) : null) : null,
    ];
  }

  function menuOf(items, info) {
    const menu = h('details', { class: 'menu' },
      h('summary', { 'aria-label': 'More actions', title: 'More actions' }, icon('dots')),
      h('div', { class: 'menu-list' }, items.map((a) => h('button', {
        type: 'button', class: a.danger ? 'menu-item danger-text' : 'menu-item', text: a.label,
        onclick: () => { menu.open = false; if (a.pick) a.pick(); else runAction(a, info.name); },
      }))));
    return menu;
  }

  function facts(info) {
    const c = info.candidate;
    const lane = laneOf(info, attention()).key;
    const link = (name) => h('a', { href: branchHref(name), text: name });
    const rows = [
      ['Status', h('span', { class: 'fact-state' }, dot(lane), cap(statusOf(info).text))],
      ['Harness', `${info.harness}${info.profile && info.profile !== info.harness ? ` (${info.profile})` : ''}`],
      info.model ? ['Model', info.model] : null,
      ['Turns', String(info.turns)],
      ['Cost', money(info.cost_usd)],
      ['Started', new Date(info.created_at * 1000).toLocaleString()],
      ['Candidate', c ? h('span', {}, h('span', { class: 'mono', text: c.commit.slice(0, 12) }), ` · ${plural(c.files_changed, 'file')} `, counts(c.insertions, c.deletions)) : 'none yet'],
      info.parent ? ['From', link(info.parent)] : null,
      info.children && info.children.length ? ['Delegated to', info.children.map((ch, i) => [i ? ', ' : '', link(ch)])] : null,
      info.superseded_by ? ['Superseded by', link(info.superseded_by)] : null,
      ['Prompt', h('span', { class: 'prompt', text: info.prompt || '' })],
    ].filter(Boolean);
    return rows.map(([k, v]) => [h('dt', { text: k }), h('dd', {}, v)]);
  }

  function updateBranch() {
    const v = view;
    const info = v && state.branches.get(v.name);
    if (!info || !$('strip')) return;
    const signature = JSON.stringify([info.status, info.stalled, info.turns, info.cost_usd, info.candidate, state.pending.get(info.name) || '',
      state.merged.has(info.name), laneOf(info, attention()).key]);
    if (signature !== v.signature) {
      const first = v.signature == null;
      v.signature = signature;
      const kept = document.activeElement === $('strip').querySelector('h1');
      fill($('strip'), ...strip(info));
      if (kept) $('strip').querySelector('h1').focus({ preventScroll: true });
      fill($('facts'), ...facts(info).flat());
      if (!first) drawTimeline(v);
    }
    applyComposer(info);
    $('changes-count').textContent = info.candidate ? String(info.candidate.files_changed) : '';
    const commit = info.candidate ? info.candidate.commit : null;
    if (commit !== v.diffFor) loadDiff(v, info);
    drawGround(v);
  }

  // The branch's files as the ground plan has them: each with the other
  // open branches that changed it too, or "only here". The tab counts the
  // files in two hands.
  function drawGround(v) {
    const slot = $('ground');
    if (view !== v || !slot) return;
    const held = groundOf(v.name);
    const info = state.branches.get(v.name);
    const closed = info && ['merged', 'discarded'].includes(info.status.state) ? info.status.state : null;
    const skipped = state.ground && state.ground.repo === state.repo ? state.ground.skipped.filter((n) => n !== v.name) : [];
    const signature = JSON.stringify([held, skipped, closed]);
    if (signature === v.groundFor) return;
    v.groundFor = signature;
    const shared = held && !closed ? held.filter((f) => f.others.length).length : 0;
    $('ground-count').textContent = shared ? String(shared) : '';
    // A merged or discarded branch holds nothing, whatever an older read says.
    if (closed) { fill(slot, h('p', { class: 'hint ground-lede', text: `${cap(closed)}, this branch holds no files.` })); return; }
    if (!held) { fill(slot, h('p', { class: 'muted', text: 'Loading…' })); return; }
    const link = (name) => h('a', { href: branchHref(name), text: name });
    fill(slot,
      h('p', { class: 'hint ground-lede', text: held.length
        ? `Which of this branch's files another open branch has changed too.${shared ? ` ${plural(shared, 'file')} in two hands.` : ''}`
        : 'This branch has changed no file, so it holds none.' }),
      held.length ? h('ul', { class: 'ground', 'aria-label': 'Who else holds these files' }, held.map((f) => {
        const at = f.path.lastIndexOf('/');
        return h('li', { class: f.others.length ? 'ground-shared' : '', 'data-path': f.path },
          h('span', { class: 'path' }, h('span', { class: 'dir', text: f.path.slice(0, at + 1) }), h('b', { text: f.path.slice(at + 1) })),
          f.others.length ? h('span', { class: 'ground-others' }, 'also in ', f.others.map((n, i) => [i ? ', ' : '', link(n)]))
            : h('span', { class: 'ground-others muted', text: 'only here' }));
      })) : null,
      skipped.length ? h('p', { class: 'hint', text: `Not counted: the diff of ${skipped.join(', ')} could not be read.` }) : null);
  }

  async function loadDiff(v, info) {
    v.diffFor = info.candidate ? info.candidate.commit : null;
    const slot = $('changes');
    const why = when.diff(info);
    if (why) { fill(slot, h('p', { class: 'muted pad', text: sentence(why, v.name) })); return; }
    if (!v.files) fill(slot, h('p', { class: 'muted pad', text: 'Loading the changes…' }));
    let text;
    try {
      text = (await api('GET', `${branchPath(v.name)}/diff`)).diff || '';
    } catch (e) {
      if (view === v) fill(slot, h('p', { class: 'error pad', text: e.message }));
      return;
    }
    if (view !== v || v.diffFor !== info.candidate.commit) return;
    v.truncated = text.length > DIFF_LIMIT ? text.length : 0;
    v.files = parseDiff(text.slice(0, DIFF_LIMIT));
    if (!v.files.some((f) => f.path === v.file)) v.file = v.files.length ? v.files[0].path : null;
    renderChanges(v, false);
  }

  function renderChanges(v, focus) {
    const slot = $('changes');
    if (!v.files.length) { fill(slot, h('p', { class: 'muted pad', text: 'The diff is empty.' })); return; }
    const file = v.files.find((f) => f.path === v.file);
    const list = h('ul', { class: 'files', id: 'files', 'aria-label': 'Changed files' }, v.files.map((f) => {
      const at = f.path.lastIndexOf('/');
      return h('li', {}, h('button', {
        type: 'button', class: 'file-row', 'aria-pressed': String(f === file), 'data-path': f.path, title: f.path,
        onclick: () => { v.file = f.path; renderChanges(v, true); },
      }, h('span', { class: 'path' }, h('span', { class: 'dir', text: f.path.slice(0, at + 1) }), h('b', { text: f.path.slice(at + 1) })),
      h('span', { class: 'file-counts' }, counts(f.add, f.del)), fileBar(f.add, f.del)));
    }));
    const pre = h('pre', { class: 'diff', tabindex: '0', 'aria-label': `Diff of ${file.path}` },
      file.lines.map((l) => h('span', { class: l.kind }, l.text)));
    const cut = file.path.lastIndexOf('/');
    // The file's header stays in view while its diff scrolls under it.
    const head = h('div', { class: 'diff-head' },
      h('span', { class: 'path' }, h('span', { class: 'dir', text: file.path.slice(0, cut + 1) }), h('b', { text: file.path.slice(cut + 1) })),
      h('span', { class: 'file-counts' }, counts(file.add, file.del)),
      v.files.length > 1 ? h('span', { class: 'diff-of muted', text: `${v.files.indexOf(file) + 1} of ${v.files.length}` }) : null);
    fill(slot,
      v.truncated ? h('p', { class: 'hint pad', text: `Showing the first ${DIFF_LIMIT} characters of ${v.truncated}.` }) : null,
      list, h('div', { class: 'diff-box' }, head, pre));
    if (focus) list.querySelector('[aria-pressed="true"]').focus();
  }

  async function loadOperations(v) {
    let operations;
    try {
      ({ operations } = await api('GET', `${repoPath()}/operations?branch=${enc(v.name)}`));
    } catch (e) {
      if (view === v) fill($('operations'), h('p', { class: 'error', text: e.message }));
      return;
    }
    if (view !== v) return;
    fill($('operations'), operations.length
      ? h('ul', { class: 'list' }, operations.map(opItem))
      : h('p', { class: 'muted', text: 'Nothing queued or running for this branch.' }));
  }

  function opItem(op) {
    return h('li', { class: 'item' },
      h('div', { class: 'item-head' }, h('span', { class: 'item-name', text: `${op.kind} ${op.branches.join(', ')}` }),
        h('span', { class: 'tag', text: op.state })),
      h('div', { class: 'item-meta' }, h('span', { class: 'mono', text: op.id }), ' · ', ago(op.created_at_ms),
        [op.priority ? `priority ${op.priority}` : null, op.waiting].filter(Boolean).map((t) => ` · ${t}`)));
  }

  // ---------------------------------------------------------------
  // One workspace: the strip with its acceptance and lifecycle actions,
  // its sessions as tabs over one conversation and message box, and an
  // inspector of its sources, outputs and details

  const workspacePath = (id) => `/v1/workspaces/${enc(id)}`;
  const workspaceHref = (id, session) => `#/w/${enc(id)}${session ? `/s/${enc(session)}` : ''}`;

  function size(n) {
    if (n == null) return '';
    if (n < 1024) return `${n} B`;
    if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
    return `${(n / 1024 / 1024).toFixed(1)} MB`;
  }

  // Short names for formats people know; any other shows its file name's
  // extension, and a row's tooltip always gives the media type in full.
  const FORMATS = {
    'text/csv': 'CSV',
    'application/vnd.openxmlformats-officedocument.spreadsheetml.sheet': 'XLSX',
    'application/vnd.openxmlformats-officedocument.wordprocessingml.document': 'DOCX',
    'application/vnd.openxmlformats-officedocument.presentationml.presentation': 'PPTX',
    'application/pdf': 'PDF',
    'application/json': 'JSON',
    'text/plain': 'Text',
    'text/markdown': 'Markdown',
    'image/png': 'PNG',
    'image/jpeg': 'JPEG',
  };
  function formatOf(type, name) {
    const known = FORMATS[(type || '').split(';')[0].trim().toLowerCase()];
    if (known) return known;
    const extension = /\.([a-z0-9]{1,8})$/i.exec(name || '');
    return extension ? extension[1].toUpperCase() : 'File';
  }

  // A source's title travels as an RFC 8187 extended value, so any name
  // arrives as written. encodeURIComponent would leave ' ( ) * bare, which
  // attr-char excludes, so each UTF-8 byte outside attr-char is escaped
  // here. X-Source-Title keeps an ASCII copy for a server that reads only it.
  const ATTR_CHAR = /^[A-Za-z0-9!#$&+\-.^_`|~]$/;
  const encodedTitle = (name) => `UTF-8''${Array.from(new TextEncoder().encode(name), (byte) => {
    const c = String.fromCharCode(byte);
    return ATTR_CHAR.test(c) ? c : `%${byte.toString(16).toUpperCase().padStart(2, '0')}`;
  }).join('')}`;
  const asciiTitle = (name) => name.replace(/[^\x20-\x7e]/gu, '_');
  // The server keeps a title to 512 UTF-8 bytes, so a longer name keeps its
  // extension and loses whole characters from the end of the rest.
  const utf8Length = (text) => new TextEncoder().encode(text).length;
  function sourceTitle(name) {
    if (utf8Length(name) <= 512) return name;
    const extension = (/\.[a-z0-9]{1,8}$/i.exec(name) || [''])[0];
    const stem = Array.from(name.slice(0, name.length - extension.length));
    while (stem.length && utf8Length(stem.join('') + extension) > 512) stem.pop();
    return stem.join('') + extension;
  }

  // A session's native status once its turn exists. Before that it has no
  // `status_detail`, and `status` is its admission operation's state.
  const sessionStatus = (s) => (s.status_detail ? { name: s.name, status: s.status_detail } : null);

  // Where a session stands, in the words every part of the page uses: its
  // tab, its outputs' heading, Details, the facts line, the conversation
  // and the next-step line. `next` is what to do about it.
  const RECOVER = 'Try again, choosing another environment or harness if this one cannot run it.';
  const PHASES = {
    queued: { label: 'Waiting to start', lane: 'running', next: 'It starts as soon as the server admits it.' },
    starting: { label: 'Starting', lane: 'running', next: 'Its first turn begins in a moment.' },
    running: { label: 'Running', lane: 'running', next: 'Steer it from the message box, or wait for the turn to end.' },
    waiting: { label: 'Waiting', lane: 'running', next: 'It continues when what it waits for finishes.' },
    interrupted: { label: 'Interrupted', lane: 'needs', next: 'Resume it from the message box.' },
    completed: { label: 'Completed', lane: 'done', next: 'Review its outputs, or continue it from the message box.' },
    failed: { label: 'Failed', lane: 'needs', next: RECOVER },
    not_started: { label: 'Did not start', lane: 'needs', next: RECOVER },
  };
  // A native state's phase, and an admission operation's before the
  // session's branch exists.
  const NATIVE_PHASE = {
    running: 'running', waiting: 'waiting', waiting_on_children: 'waiting', interrupted: 'interrupted',
    ready: 'completed', no_changes: 'completed', merged: 'completed',
    failed: 'failed', blocked: 'failed', budget_exceeded: 'failed', awaiting_plan_approval: 'failed',
  };
  const ADMISSION_PHASE = { queued: 'queued', running: 'starting', succeeded: 'starting', failed: 'not_started', interrupted: 'not_started' };
  const ACTIVE_PHASES = ['queued', 'starting', 'running', 'waiting'];
  const RECOVERABLE_PHASES = ['not_started', 'failed'];

  function sessionPhase(s) {
    const native = s.status_detail;
    const key = native ? NATIVE_PHASE[native.state] || 'running' : ADMISSION_PHASE[s.status] || 'starting';
    const phase = PHASES[key];
    let lane = phase.lane;
    let detail = null;
    if (key === 'not_started') detail = s.error ? s.error.message : null;
    else if (key === 'completed' && native.state === 'ready') lane = 'ready';
    else if (key === 'completed' || (key === 'failed' && native.state !== 'failed')) detail = statusOf({ status: native }).text;
    else if (key === 'failed') detail = native.reason || null;
    return { key, label: phase.label, lane, next: phase.next, detail };
  }
  const unstarted = (s) => sessionPhase(s).key === 'not_started';
  const sessionActive = (s) => ACTIVE_PHASES.includes(sessionPhase(s).key);
  const sessionLane = (s) => sessionPhase(s).lane;
  function sessionText(s) {
    const { label, detail } = sessionPhase(s);
    return detail ? `${label}: ${detail}` : label;
  }
  const sessionsOf = (v) => (v.workspace.sessions || []).slice().sort((x, y) => x.created_at_ms - y.created_at_ms);
  const selectedSession = (v) => sessionsOf(v).find((s) => s.id === v.session) || null;

  // The message box's verb for the selected session, by the branch rules:
  // a running turn steers, an interrupted one resumes, any other sends.
  const SESSION_COMPOSER = {
    send: {
      action: 'send', label: 'Send', title: 'Send a follow-up', rule: when.send,
      placeholder: (s) => `Follow up in ${s.name}…`, started: 'Sent. A new turn is starting.',
    },
    steer: {
      action: 'steer', label: 'Steer', title: 'Steer the running turn', rule: when.steer,
      placeholder: (s) => `Steer ${s.name}'s running turn…`, started: 'Steering sent. The running turn reads it next.',
    },
    resume: {
      action: 'resume', label: 'Resume', title: 'Resume the interrupted turn', rule: when.resume, empty: RESUME_PROMPT,
      placeholder: () => 'Resume as it was, or say what to do next…', started: 'Resuming. A new turn is starting.',
    },
  };

  function workspaceCanRun(v) {
    const ws = v.workspace;
    const caps = ws.capabilities || {};
    return can('run') && !v.gone && caps.can_run !== false &&
      (!ws.current_role || ['owner', 'editor'].includes(ws.current_role));
  }

  function workspaceCanComment(v) {
    const ws = v.workspace;
    const caps = ws.capabilities || {};
    return can('run') && !v.gone && caps.can_comment !== false &&
      (!ws.current_role || ['owner', 'editor', 'reviewer'].includes(ws.current_role));
  }

  function sessionCanContinue(v, s) {
    return s && s.access === 'reviewer' ? workspaceCanComment(v) : workspaceCanRun(v);
  }

  function sessionComposer(v, s) {
    const info = s && sessionStatus(s);
    const st = info && info.status.state;
    const mode = st === 'running' ? SESSION_COMPOSER.steer : st === 'interrupted' ? SESSION_COMPOSER.resume : SESSION_COMPOSER.send;
    let why = null;
    if (!sessionCanContinue(v, s)) why = 'this workspace does not permit continuing this session';
    else if (!s) why = 'start a session first';
    else if (!['ready', 'running'].includes(v.workspace.state)) why = `the workspace is ${workspaceStateText(v.workspace)}`;
    else if (unstarted(s)) why = `${s.name} did not start${s.error ? `: ${s.error.message}` : ''}`;
    else if (!info) why = `${s.name} is starting; its first turn has not begun`;
    else if ((v.workspace.current_subject || state.me.subject) && s.subject && s.subject !== (v.workspace.current_subject || state.me.subject)) why = `only ${s.subject} can continue ${s.name}`;
    else why = mode.rule(info);
    return { mode, why };
  }

  // The workspace's rare actions, in the ⋯ menu. `when` is false when an
  // action does not apply now (not shown), the server's reason when it
  // reports the capability missing (shown disabled, with that reason), or
  // null when it can be asked for.
  const WORKSPACE_MENU = [
    {
      label: 'Cancel turn', danger: true, run: (v, s) => cancelSession(v, s),
      when: (v, s) => (s && sessionActive(s) && sessionCanContinue(v, s) ? null : false),
    },
    {
      label: 'Archive', run: (v) => lifecycle(v, 'archive'),
      when: (v) => (v.workspace.state === 'ready' && workspaceCanRun(v) ? null : false),
    },
    {
      label: 'Restore', run: (v) => lifecycle(v, 'restore'),
      when: (v) => (['archived', 'failed', 'interrupted'].includes(v.workspace.state) && workspaceCanRun(v) ? null : false),
    },
    { label: 'Sleep', run: (v) => lifecycle(v, 'sleep'), when: (v) => capability(v, 'sleep') },
  ];

  function capability(v, name) {
    const c = (v.workspace.capabilities || {})[name];
    if (!c || v.workspace.state !== 'ready' || !workspaceCanRun(v)) return false;
    return c.available ? null : c.reason || 'not available';
  }

  // Why a workspace takes no new session, by its state (or, for a view
  // state such as `running`, the lifecycle state behind it); null when it
  // takes one.
  const NOT_READY = {
    pending: 'The workspace is starting; sessions can start once it is ready.',
    restoring: 'The workspace is being restored; sessions can start once it is ready.',
    archiving: 'The workspace is being archived.',
    archived: 'The workspace is archived: restore it from ⋯, then start.',
    failed: 'The workspace\'s setup failed: restore it from ⋯, then start.',
    interrupted: 'The workspace\'s setup was interrupted: restore it from ⋯, then start.',
  };
  const notReady = (ws) => NOT_READY[ws.state] || NOT_READY[ws.lifecycle_intent] || null;
  const said = (text) => (/[.!?]$/.test(text) ? cap(text) : `${cap(text)}.`);

  // Readiness in fixed words: a state's word and the dot that repeats it,
  // and where it was measured. Text is always shown, and readiness never
  // disables anything.
  const READY = { ready: ['Ready', 'ready'], not_ready: ['Not ready', 'needs'], unknown: ['Unknown', 'done'] };
  const SCOPE = { api_host: 'checked on this server', environment: 'in the environment itself' };
  // Whether an environment supplies a harness's key is configuration, not a
  // measurement: its row has a fact and no state.
  const KEY = { configured: 'This environment supplies its key', none: 'This environment supplies no key' };
  const PROVIDERS = { local: 'Local', substrate: 'Substrate', microsandbox: 'Microsandbox' };
  const EFFORTS = ['low', 'medium', 'high', 'xhigh'];

  const readinessRow = (name, r) => ({
    name, state: r.state, scope: r.scope, reason: r.reason, action: r.action, checkedMsAgo: r.checked_ms_ago == null ? null : r.checked_ms_ago,
  });

  // Discovery's presets as the environments a session can start in. Which
  // `preset` a start sends is decided here and nowhere else: the server's
  // implicit default environment sends none (today's start, so coding
  // projects are unchanged) unless the server requires one.
  function environmentsOf(list) {
    return list.presets.map((p) => {
      const preset = p.origin === 'default' && !list.required ? null : p.id;
      return {
        key: p.id, label: p.label, description: p.description || '', provider: p.provider, origin: p.origin, preset,
        canStart: p.can_start !== false,
        readiness: [readinessRow('Environment', p.environment), p.reachability ? readinessRow('Reachability', p.reachability) : null].filter(Boolean),
        harnesses: p.harnesses.map((x) => ({
          id: x.harness, models: x.models || null, defaultModel: x.default_model || null, freeModel: preset === null,
          toolApprovals: x.tool_approvals, writer: x.writer, reviewer: x.reviewer, effort: x.effort === true, credentials: x.credentials,
          readiness: [readinessRow('Executable', x.executable), readinessRow('Sign-in', x.auth), { name: 'Key', fact: KEY[x.credentials] || x.credentials }],
        })),
      };
    });
  }

  // The environment a session started in: its preset's, or the default
  // environment's when it sent none.
  const environmentOf = (v, s) => v.environments.list.find((env) => env.preset === (s.preset || null)) || null;

  const WRITER_CANNOT = 'it cannot ask before each tool, and this server does not accept that for writers';
  const REVIEWER_CANNOT = 'reviewers need a harness that routes each tool request for approval';
  // Why each access is closed for a harness and the caller's role, or null.
  const accessWhy = (x, roles) => ({
    writer: !roles.run ? 'your role in this workspace cannot start writers' : x.writer === false ? WRITER_CANNOT : null,
    reviewer: !roles.comment ? 'your role in this workspace cannot start reviewers' : x.reviewer === false ? REVIEWER_CANNOT : null,
  });

  // Everything the new-session form offers for a draft, what is closed and
  // why, and the first thing still missing. Pure: the form draws it and
  // Start checks it.
  function startChoices(environments, roles, draft, outputs) {
    const envs = environments.list.map((env) => ({ env, why: env.canStart ? null : 'Your role in this workspace cannot start sessions here.' }));
    const open = envs.filter((e) => !e.why);
    const picked = open.find((e) => e.env.key === draft.env) || (open.length === 1 ? open[0] : null);
    const env = picked ? picked.env : null;
    const harnesses = (env ? env.harnesses : []).map((x) => {
      const w = accessWhy(x, roles);
      const why = w.writer && w.reviewer ? [roles.run ? w.writer : null, roles.comment ? w.reviewer : null].filter(Boolean).join('; ') || w.writer : null;
      return { harness: x, why };
    });
    const usable = harnesses.filter((x) => !x.why);
    // The default environment of a server that lists no harness keeps
    // today's typed harness.
    const typed = !!env && env.preset === null && !env.harnesses.length;
    const typedHarness = draft.harness && draft.harness.trim();
    const chosen = usable.find((x) => x.harness.id === draft.harness) || usable[0];
    const harness = typed
      ? (typedHarness ? { id: typedHarness, models: null, freeModel: true, toolApprovals: null, writer: null, reviewer: null, effort: false, readiness: [] } : null)
      : chosen ? chosen.harness : null;
    const base = accessWhy(harness || {}, roles);
    const other = draft.access === 'writer' ? 'reviewer' : 'writer';
    const value = base[draft.access] && !base[other] ? other : draft.access;
    // `fixed` when the role or the harness closes the other access, so the
    // choice is not the person's; `forced` when that leaves reviewer.
    const fixed = !!base[value === 'writer' ? 'reviewer' : 'writer'];
    const access = {
      value, writer: base.writer, reviewer: base.reviewer || (outputs.length ? null : 'no outputs to review yet'),
      fixed, forced: value === 'reviewer' && fixed,
    };
    let model = { kind: 'none', options: [], value: '' };
    if (harness && harness.freeModel) model = { kind: 'free', options: [], value: draft.model };
    else if (harness && harness.models && harness.models.length) {
      model = { kind: 'select', options: harness.models, value: harness.models.includes(draft.model) ? draft.model : harness.defaultModel || harness.models[0] };
    }
    const offered = !!harness && harness.effort;
    const effort = { offered, value: offered && EFFORTS.includes(draft.effort) ? draft.effort : '' };
    const acknowledgement = value === 'writer' && !!harness && harness.toolApprovals === false;
    // The first thing the start still needs, in the form's order.
    const firstMissing = () => {
      if (environments.state !== 'ready') {
        return { field: 'environments', text: environments.state === 'failed' ? `The environments did not load: ${said(environments.error)} Retry, then start.` : 'Loading the environments this server offers…' };
      }
      if (!env) return { field: 'environment', text: open.length ? 'Choose where it runs.' : 'No environment here can start a session for your role.' };
      if (!harness) {
        return { field: 'harness', text: typed ? 'Type a harness this server runs.' : usable.length ? 'Choose a harness.' : `No harness in ${env.label} can start a session for your role; choose another environment.` };
      }
      if (acknowledgement && !draft.ack) return { field: 'ack', text: `Tick “I accept tools without per-tool approval” to start ${harness.id} as a writer, or choose another harness.` };
      if (!draft.name.trim()) return { field: 'name', text: 'Name the session.' };
      if (!draft.prompt.trim()) return { field: 'prompt', text: 'Give it a first prompt.' };
      if (value === 'reviewer' && !outputs.some((a) => draft.outputs.has(a.id))) {
        return { field: 'outputs', text: outputs.length ? 'Choose at least one output to review (under Advanced).' : 'No outputs to review yet: a reviewer needs at least one.' };
      }
      return null;
    };
    return { envs, env, harnesses, harness, typed, access, model, effort, acknowledgement, missing: firstMissing() };
  }

  // The body of a start for a draft whose choices miss nothing.
  function startRequest(c, draft, sources, outputs) {
    const writer = c.access.value === 'writer';
    const body = { name: draft.name.trim(), prompt: draft.prompt.trim(), harness: c.harness.id, access: c.access.value };
    if (c.env.preset !== null) body.preset = c.env.preset;
    const model = c.model.kind === 'none' ? '' : c.model.value.trim();
    if (model) body.model = model;
    if (c.effort.offered && c.effort.value) body.effort = c.effort.value;
    const ticked = sources.filter((src) => draft.sources.get(src.id) !== false).map((src) => src.id);
    if (ticked.length) body.sources = ticked;
    if (!writer) body.artifacts = outputs.filter((a) => draft.outputs.has(a.id)).map((a) => a.id);
    if (writer && draft.collect) body.result_contract = OUTPUT_CONTRACT;
    if (writer && draft.allowAll) body.policy = { mode: 'allow' };
    if (writer && c.acknowledgement && draft.ack) body.unapproved_tools = true;
    return body;
  }

  // A new session's draft. A source missing from `sources` has not been
  // seen by the draft, so it starts ticked.
  function freshDraft(v, keep = {}) {
    return {
      name: '', prompt: '', env: keep.env || null, harness: keep.harness || null, model: keep.model || '', effort: '',
      access: workspaceCanRun(v) ? 'writer' : 'reviewer', sources: new Map(), outputs: new Set(),
      collect: v.workspace.result_mode !== 'code', allowAll: false, ack: false,
    };
  }

  // A draft that starts `s` again: its first prompt from its messages, or
  // from what this tab sent when it never ran. Consents (the tool
  // acknowledgement, Allow every tool) are asked for again.
  function draftFrom(v, s) {
    const m = v.messages.get(s.id);
    const first = m && m.list.find((x) => x.role === 'user');
    const sent = v.sent.get(s.id);
    const env = environmentOf(v, s);
    return {
      ...freshDraft(v),
      name: `${s.name} (again)`, prompt: first ? first.text : sent ? sent.prompt : '',
      env: env ? env.key : null, harness: s.harness, model: s.model || '', effort: s.effort || '', access: s.access || 'writer',
      sources: new Map(sourcesOf(v).map((src) => [src.id, (s.sources || []).includes(src.id)])),
      outputs: new Set((s.artifacts || []).map((a) => a.id)),
      collect: s.result_contract != null,
    };
  }

  const sourcesOf = (v) => (v.workspace.sources || []).slice().sort((x, y) => x.created_at_ms - y.created_at_ms);

  // The one next useful step for this workspace and person, first match
  // first, with at most one button.
  const needsAttention = (v) => sessionsOf(v).find((s) => ['not_started', 'failed', 'interrupted'].includes(sessionPhase(s).key)) || null;
  const awaitingReview = (v) => (v.artifacts ? outputGroups(v).flatMap((g) => g.rows).filter((r) => r.standing === 'current').length : 0);
  const NEXT_STEPS = [
    { when: (v) => !!notReady(v.workspace), text: (v) => notReady(v.workspace), action: () => null },
    {
      when: (v) => !workspaceCanRun(v) && !workspaceCanComment(v),
      text: () => 'You can read and download here; an owner can make you an editor.', action: () => null,
    },
    {
      when: (v) => v.workspace.result_mode !== 'code' && !sourcesOf(v).length && workspaceCanRun(v),
      text: () => 'Upload the files its sessions should read, such as a spreadsheet, a document or a CSV.',
      action: () => ({ label: 'Upload sources', run: () => $('source-file').click() }),
    },
    {
      when: (v) => !sessionsOf(v).length, text: () => 'Start its first session.',
      action: (v) => (v.composing ? null : { label: '+ Session', run: () => composeSession(v) }),
    },
    { when: (v) => v.selected.size > 0, text: (v) => `${plural(v.selected.size, 'output')} ticked: ready to accept.`, action: () => null },
    {
      when: (v) => !!needsAttention(v),
      text: (v) => {
        const s = needsAttention(v);
        const phase = sessionPhase(s);
        return `${s.name}: ${phase.label.toLowerCase()}. ${phase.next}`;
      },
      action: (v) => {
        const s = needsAttention(v);
        return s.id === v.session && !v.composing ? null : { label: `Open ${s.name}`, run: () => go(workspaceHref(v.id, s.id)) };
      },
    },
    {
      when: (v) => awaitingReview(v) > 0, text: (v) => `${plural(awaitingReview(v), 'output')} waiting for review.`,
      action: (v) => (v.tab === 'outputs' ? null : { label: 'Review outputs', run: () => selectWorkspaceTab(v, 'outputs') }),
    },
    { when: (v) => sessionsOf(v).some(sessionActive), text: () => 'Sessions are running: outputs appear under each session when its turn ends.', action: () => null },
    { when: () => true, text: () => 'Continue a session from the message box, or start another with + Session.', action: () => null },
  ];
  function nextStep(v) {
    const step = NEXT_STEPS.find((s) => s.when(v));
    return { text: step.text(v), action: step.action(v) };
  }

  // A refused start, by the server's code: what to say and what to read
  // again first. The draft is never touched.
  const OPERATOR_REFUSED = { text: (e) => `${said(e.message)} Choose another environment or harness, or ask the operator.` };
  const RELOAD_PRESETS = { reload: 'environments', text: () => 'Start in one of the environments this server offers. They are reloaded; choose one.' };
  const START_REFUSED = {
    preset_choice_unavailable: {
      reload: 'environments',
      text: (e) => {
        const d = e.detail || {};
        const allowed = (d.allowed || []).length ? `It offers ${d.allowed.join(', ')}.` : 'It offers none.';
        return `This environment does not offer that ${d.field || 'choice'}. ${allowed} The environments are reloaded; choose again.`;
      },
    },
    unknown_preset: { reload: 'environments', text: () => 'That environment is no longer offered. The environments are reloaded; choose another.' },
    preset_required: RELOAD_PRESETS,
    preset_conflict: RELOAD_PRESETS,
    workspace_not_ready: { reload: 'workspace', text: (e, v) => `${notReady(v.workspace) || said(e.message)} Your draft is kept.` },
    quota_exceeded: { text: (e) => `${said(e.message)} Wait for a running session to finish, or ask the operator for more room. Your draft is kept.` },
    provider_not_allowed: OPERATOR_REFUSED,
    unapproved_tools_not_allowed: OPERATOR_REFUSED,
    unsupported: OPERATOR_REFUSED,
    forbidden: OPERATOR_REFUSED,
  };

  // The open workspace, kept across polls and session switches.
  let wsView = null;

  function closeWorkspace() {
    if (wsView) clearTimeout(wsView.timer);
    wsView = null;
  }

  // The one live preview, of a source or an output, disposed on every route,
  // tab, item and source change and on signing out, so no hidden preview
  // keeps fetching.
  let livePreview = null;
  function closePreview() {
    closeDeckPreview();
    if (livePreview) livePreview.dispose();
    livePreview = null;
    if (!wsView) return;
    wsView.previewing = null;
    const slot = $('preview');
    if (slot) fill(slot);
    for (const b of document.querySelectorAll('#inspector button[data-preview]')) b.setAttribute('aria-pressed', 'false');
  }

  // Every write the server checks against the workspace's revision reads it
  // here. A stale revision (or a write that lost a race for the record)
  // reloads the workspace and asks the person to try again: no write is
  // retried behind their back.
  async function revised(v, what, write) {
    try {
      return await write(v.workspace.revision);
    } catch (e) {
      if (e.status !== 409 || e.code !== 'revision_conflict') throw e;
      let reloaded = true;
      try { await reloadDetail(v); } catch (_) { reloaded = false; }
      if (wsView === v) drawWorkspace(v);
      toast(reloaded
        ? `${what}: the workspace changed meanwhile. It is reloaded at revision ${v.workspace.revision}; try again.`
        : `${what}: the workspace changed meanwhile, and reloading it failed. Reload the page, then try again.`);
      return null;
    }
  }

  async function renderWorkspace() {
    const { workspace: id, session } = state.route;
    if (wsView && wsView.id === id && $('workspace')) {
      selectSession(wsView, session);
      return;
    }
    const v = {
      id, workspace: null, session: null, messages: new Map(),
      artifacts: null, artifactsError: null, artifactsFor: null,
      selected: new Set(), previewing: null, tab: 'sources',
      composing: false, busy: null, sending: false, following: 0, drafts: new Map(),
      timer: null, refreshing: false, again: false, failures: 0, failure: null,
      asked: 0, applied: 0, drawn: {},
      // The new session's draft, the body of every start this tab sent (so
      // Try again has a prompt that never ran), and the environments.
      draft: null, sent: new Map(), syncForm: null,
      environments: { state: 'loading', list: [], error: null, at: 0 }, environmentsAsked: 0,
    };
    wsView = v;
    main(h('div', { class: 'page' }, h('p', { class: 'muted', role: 'status', text: 'Opening the workspace…' })));
    try {
      await reloadDetail(v);
    } catch (e) {
      if (wsView !== v) return;
      main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Back' }),
        h('h1', { text: 'Workspace not available' }), h('p', { class: 'error', text: e.message })));
      return;
    }
    if (wsView !== v) return;
    workspaceLayout(v);
    selectSession(v, session);
    loadArtifacts(v).then(() => { if (wsView === v) drawWorkspace(v); });
    loadEnvironments(v);
    scheduleWorkspace(v);
  }

  // The environments a session here can start in. A later read replaces
  // an earlier one, never the reverse; a reload keeps the list shown until
  // it answers.
  async function loadEnvironments(v) {
    const asked = (v.environmentsAsked += 1);
    v.environments = { ...v.environments, state: 'loading', error: null };
    if (v.composing && v.syncForm) v.syncForm();
    let next;
    try {
      const list = await api('GET', `/v1/execution-presets?workspace=${enc(v.id)}`);
      next = { state: 'ready', list: environmentsOf(list), error: null, at: Date.now() };
    } catch (e) {
      next = { state: 'failed', list: [], error: e.message, at: 0 };
    }
    if (wsView !== v || asked !== v.environmentsAsked) return;
    v.environments = next;
    drawWorkspace(v);
  }

  // Reads overlap (a poll, an upload's reload): one asked earlier never
  // replaces one asked later. Outputs are read again whenever a session's
  // turn or status moved since they were last read.
  async function reloadDetail(v) {
    const asked = (v.asked += 1);
    const ws = await api('GET', workspacePath(v.id));
    if (wsView !== v || asked < v.applied) return;
    v.applied = asked;
    v.workspace = ws;
    prune(v);
    if (!projectOf(ws.project_id)) {
      const project = await api('GET', `/v1/projects/${enc(ws.project_id)}`);
      state.projects = (state.projects || []).filter((p) => p.id !== project.id).concat(project);
      if (wsView !== v) return;
    }
    useProject(ws.project_id);
    const row = state.workspaces.list.find((w) => w.id === ws.id);
    if (row && (row.state !== ws.state || row.title !== ws.title)) {
      Object.assign(row, { state: ws.state, title: ws.title, revision: ws.revision });
      workspacesChanged();
    } else if (!row && state.workspaces.loaded && state.workspaces.project === ws.project_id) {
      loadWorkspaces(ws.project_id);
    }
    if (v.artifacts !== null && turnsOf(v) !== v.artifactsFor) await loadArtifacts(v);
  }

  const INSPECTOR = [['sources', 'Sources', 'sources-count'], ['outputs', 'Outputs', 'outputs-count'], ['details', 'Details']];

  function workspaceLayout(v) {
    const files = h('input', { type: 'file', id: 'source-file', multiple: true, hidden: true, tabindex: '-1' });
    files.addEventListener('change', () => {
      const picked = Array.from(files.files);
      files.value = '';
      if (picked.length) uploadSources(v, picked);
    });
    const drop = h('div', { class: 'drop', id: 'source-drop', text: 'Or drop files here' });
    drop.addEventListener('dragover', (ev) => { ev.preventDefault(); drop.classList.add('over'); });
    drop.addEventListener('dragleave', () => drop.classList.remove('over'));
    drop.addEventListener('drop', (ev) => {
      ev.preventDefault();
      drop.classList.remove('over');
      const picked = Array.from((ev.dataTransfer && ev.dataTransfer.files) || []);
      if (picked.length && workspaceCanRun(v)) uploadSources(v, picked);
    });
    main(h('div', { class: 'branch workspace', id: 'workspace' },
      h('section', { class: 'focus', 'aria-label': 'Conversation' },
        h('header', { class: 'strip', id: 'strip' }),
        h('div', { class: 'guide', id: 'guide', role: 'status' }),
        h('nav', { class: 'sessions', id: 'sessions', 'aria-label': 'Sessions' }),
        h('ul', { class: 'session-facts', id: 'session-facts', 'aria-label': 'Selected session', hidden: true }),
        h('div', { class: 'convo', id: 'convo' }),
        composer(() => submitSession(v))),
      h('aside', { class: 'inspector', id: 'inspector', 'aria-label': 'Inspector' },
        tablist(INSPECTOR, () => v.tab, (tab) => selectWorkspaceTab(v, tab)),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-sources', 'aria-labelledby': 'tab-sources' },
          h('div', { class: 'panel-tools' },
            h('button', { type: 'button', id: 'upload-sources', text: 'Upload sources', disabled: !workspaceCanRun(v), onclick: () => files.click() }),
            files, workspaceCanRun(v) ? drop : null),
          h('ul', { class: 'files file-list', id: 'sources', 'aria-label': 'Sources' }),
          h('div', { class: 'preview', id: 'preview', 'aria-label': 'Preview' })),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-outputs', 'aria-labelledby': 'tab-outputs', hidden: true },
          h('div', { id: 'outputs-list' })),
        h('div', { class: 'panel', role: 'tabpanel', id: 'panel-details', 'aria-labelledby': 'tab-details', hidden: true },
          h('dl', { class: 'facts', id: 'ws-facts' }),
          h('h2', { text: 'Sessions' }), h('ul', { class: 'events', id: 'ws-sessions' }),
          h('h2', { text: 'Members' }), h('ul', { class: 'events', id: 'ws-members' })))));
  }

  function selectWorkspaceTab(v, tab) {
    closePreview();
    v.tab = tab;
    showTab(INSPECTOR, tab);
    if (tab === 'outputs' && v.artifacts === null) loadArtifacts(v).then(() => { if (wsView === v) drawOutputs(v); });
    drawWorkspace(v);
  }

  // The session the route names, else the most recent; a workspace with no
  // session opens the form for its first.
  function selectSession(v, id) {
    const sessions = sessionsOf(v);
    const chosen = sessions.find((s) => s.id === id) || sessions[sessions.length - 1] || null;
    const area = $('composer-text');
    if (v.session !== (chosen && chosen.id)) {
      if (v.session) v.drafts.set(v.session, area.value);
      area.value = (chosen && v.drafts.get(chosen.id)) || '';
    }
    v.session = chosen ? chosen.id : null;
    if (v.composing || !chosen) {
      v.composing = !chosen && (workspaceCanRun(v) || workspaceCanComment(v)) && !notReady(v.workspace);
      v.drawn.convo = null;
    }
    drawWorkspace(v);
    if (v.composing) openSessionForm(v);
    if (chosen && sessionStatus(chosen)) loadMessages(v, chosen.id).then(() => { if (wsView === v) drawWorkspace(v); });
  }

  // A session's messages page by page from its cursor. The server reads
  // the native log a page at a time, so a page may hold no message while
  // the cursor still moves.
  function loadMessages(v, id) {
    let m = v.messages.get(id);
    if (!m) {
      m = { list: [], cursor: '0', loaded: false, error: null, loading: null };
      v.messages.set(id, m);
    }
    if (m.loading) return m.loading;
    m.loading = (async () => {
      try {
        for (;;) {
          const page = await api('GET', `${workspacePath(v.id)}/sessions/${enc(id)}/messages?after=${enc(m.cursor)}`);
          m.list.push(...page.messages);
          const moved = page.next_cursor !== m.cursor;
          m.cursor = page.next_cursor;
          if (!moved) break;
        }
        m.loaded = true;
        m.error = null;
      } catch (e) {
        m.error = e.message;
      } finally {
        m.loading = null;
      }
    })();
    return m.loading;
  }

  const turnsOf = (v) => JSON.stringify((v.workspace.sessions || []).map((s) => [s.id, s.status, s.turns]));
  async function loadArtifacts(v) {
    const sessions = v.workspace.sessions || [];
    v.artifactsFor = turnsOf(v);
    if (!sessions.length) { v.artifacts = []; return; }
    try {
      v.artifacts = (await api('GET', `${workspacePath(v.id)}/artifacts`)).artifacts;
      v.artifactsError = null;
    } catch (e) {
      v.artifactsError = e.message;
      if (v.artifacts === null) v.artifacts = [];
    }
    prune(v);
  }

  // While anything here is starting, running or settling, the page reads
  // the workspace again every second and a half; managed workspaces have
  // no event stream to listen to.
  function scheduleWorkspace(v, delay = 1500) {
    clearTimeout(v.timer);
    if (wsView !== v || !v.workspace || v.gone) return;
    const busy = v.following > 0 || workspaceLane(v.workspace).key === 'running' || (v.workspace.sessions || []).some(sessionActive);
    if (busy) v.timer = setTimeout(() => refreshWorkspace(v), Math.min(30000, delay * 2 ** v.failures));
  }

  async function refreshWorkspace(v) {
    if (wsView !== v) return;
    if (v.refreshing) { v.again = true; return; }
    v.refreshing = true;
    try {
      await reloadDetail(v);
      const s = wsView === v ? selectedSession(v) : null;
      if (s && sessionStatus(s)) await loadMessages(v, s.id);
      v.failures = 0;
      v.failure = null;
    } catch (e) {
      v.failures += 1;
      v.gone = e.status === 403 || e.status === 404;
      v.failure = v.gone ? `This workspace is no longer open to this token: ${e.message}` : `Could not refresh: ${e.message}`;
    } finally {
      v.refreshing = false;
    }
    if (wsView !== v || !state.token) return;
    drawWorkspace(v);
    if (v.again) { v.again = false; refreshWorkspace(v); return; }
    scheduleWorkspace(v);
  }

  // Watch an operation this page started; the workspace is read again as
  // it settles, and a failure (or `finished`) is said.
  async function follow(v, op, label, finished) {
    v.following += 1;
    scheduleWorkspace(v, 300);
    const done = await awaitOperation(op.id);
    v.following -= 1;
    if (done && done.state !== 'succeeded') toast(`${label} ${done.state}${done.error ? `: ${done.error.message}` : '.'}`);
    else if (done && finished) toast(finished);
    if (wsView === v) refreshWorkspace(v);
  }

  function drawWorkspace(v) {
    if (wsView !== v || !$('strip')) return;
    drawStrip(v);
    drawGuide(v);
    drawSessions(v);
    drawSessionFacts(v);
    if (v.composing && v.syncForm) v.syncForm();
    $('upload-sources').disabled = !workspaceCanRun(v);
    const drop = $('source-drop');
    if (drop) drop.hidden = !workspaceCanRun(v);
    drawConversation(v);
    applySessionComposer(v);
    drawSources(v);
    drawOutputs(v);
    drawDetails(v);
  }

  function drawStrip(v) {
    const ws = v.workspace;
    const s = selectedSession(v);
    const project = projectOf(ws.project_id);
    const lane = workspaceLane(ws);
    const items = WORKSPACE_MENU.map((item) => ({ item, why: item.when(v, s) })).filter(({ why }) => why !== false);
    const n = v.selected.size;
    const signature = JSON.stringify([ws.title, ws.state, ws.revision, project && project.name, n, v.busy, v.failure, s && s.id, items.map(({ item, why }) => [item.label, why])]);
    if (signature === v.drawn.strip) return;
    const strip = $('strip');
    if (strip.querySelector('details.menu[open]')) return;
    v.drawn.strip = signature;
    const kept = document.activeElement === strip.querySelector('h1');
    const focused = strip.contains(document.activeElement) && document.activeElement.id;
    let action = null;
    if (v.busy) action = h('button', { type: 'button', class: 'primary', disabled: true, text: v.busy });
    else if (n) action = h('button', { type: 'button', class: 'primary', id: 'accept', text: `Accept ${n} selected`, disabled: !workspaceCanRun(v), onclick: () => acceptSelected(v) });
    fill(strip,
      h('a', { class: 'back', href: `#/p/${enc(ws.project_id)}`, text: '‹ Workspaces' }),
      h('div', { class: 'strip-title' }, h('h1', { tabindex: '-1', text: ws.title }),
        h('p', { class: 'strip-meta', text: `${project ? project.name : 'Project'} · revision ${ws.revision}` })),
      h('p', { class: `state state-${lane.key}` }, dot(lane.key), cap(workspaceStateText(ws))),
      action,
      items.length ? workspaceMenu(v, s, items) : null,
      v.failure ? h('p', { class: 'error strip-error', role: 'status', text: v.failure }) : null);
    if (kept) strip.querySelector('h1').focus({ preventScroll: true });
    else if (focused && $(focused)) $(focused).focus({ preventScroll: true });
  }

  function workspaceMenu(v, s, items) {
    const menu = h('details', { class: 'menu' },
      h('summary', { 'aria-label': 'More actions', title: 'More actions' }, icon('dots')),
      h('div', { class: 'menu-list' }, items.map(({ item, why }) => h('button', {
        type: 'button', class: item.danger ? 'menu-item danger-text' : 'menu-item', disabled: why != null,
        onclick: () => { menu.open = false; item.run(v, s); },
      }, h('span', { text: item.label }), why != null ? h('span', { class: 'menu-why', text: `Not available here: ${why}` }) : null))));
    menu.addEventListener('toggle', () => { if (!menu.open && wsView === v) drawStrip(v); });
    return menu;
  }

  function drawSessions(v) {
    const sessions = sessionsOf(v);
    const signature = JSON.stringify([sessions.map((s) => [s.id, s.name, sessionLane(s), sessionText(s)]), v.session, v.composing, workspaceCanRun(v), workspaceCanComment(v)]);
    if (signature === v.drawn.sessions) return;
    v.drawn.sessions = signature;
    fill($('sessions'), sessions.map((s) => h('a', {
      class: 'session-tab', href: workspaceHref(v.id, s.id), 'data-session': s.id, title: `${s.name}: ${sessionText(s)}`,
      'aria-current': s.id === v.session && !v.composing ? 'page' : null,
      onclick: () => { if (v.composing) { v.composing = false; v.drawn.convo = null; drawWorkspace(v); } },
    }, dot(sessionLane(s)), h('span', { class: 'session-name', text: s.name }))),
    workspaceCanRun(v) || workspaceCanComment(v) ? h('button', {
      type: 'button', class: 'ghost new-session', id: 'new-session', 'aria-pressed': String(v.composing), text: '+ Session',
      onclick: () => composeSession(v),
    }) : null);
  }

  function composeSession(v) {
    v.composing = true;
    v.drawn.convo = null;
    drawWorkspace(v);
    openSessionForm(v);
  }

  // Try again: the form, with a draft taken from `s`.
  function tryAgain(v, s) {
    v.draft = draftFrom(v, s);
    composeSession(v);
  }

  // The next useful step, redrawn only when what it says changes, so it is
  // announced once.
  function drawGuide(v) {
    const { text, action } = nextStep(v);
    const signature = JSON.stringify([text, action && action.label]);
    if (signature === v.drawn.guide) return;
    v.drawn.guide = signature;
    const guide = $('guide');
    const focused = guide.contains(document.activeElement);
    fill(guide, h('span', { class: 'guide-text', text }),
      action ? h('button', { type: 'button', id: 'guide-action', text: action.label, onclick: action.run }) : null);
    if (focused) ($('guide-action') || $('workspace').querySelector('h1')).focus({ preventScroll: true });
  }

  // What the selected session is: where it runs, on what, what it reads,
  // and where it stands. Hidden while composing.
  function drawSessionFacts(v) {
    const s = v.composing ? null : selectedSession(v);
    const box = $('session-facts');
    const phase = s && sessionPhase(s);
    const env = s && environmentOf(v, s);
    let where = null;
    if (env) where = `${env.label} (${PROVIDERS[env.provider] || env.provider})`;
    else if (s && s.preset) where = s.preset;
    else if (s && v.environments.state === 'ready') where = 'started without a preset';
    const facts = s ? [
      where, s.harness, s.model || 'harness default model', s.access,
      s.access === 'reviewer' ? `reviews ${plural((s.artifacts || []).length, 'output')}` : null,
      `reads ${plural((s.sources || []).length, 'source')}`,
      s.access === 'reviewer' ? null : s.result_contract ? 'collects outputs' : 'collects no outputs',
    ].filter(Boolean) : [];
    const signature = JSON.stringify([facts, phase && [phase.label, phase.lane]]);
    if (signature === v.drawn.facts) return;
    v.drawn.facts = signature;
    box.hidden = !s;
    fill(box, facts.map((text) => h('li', { text })), s ? h('li', { class: 'fact-phase' }, dot(phase.lane), phase.label) : null);
  }

  // Messages are turned into turns by the branch conversation's reducer:
  // what the person sent opens a turn and the agent's text joins it.
  function drawConversation(v) {
    if (v.composing) return;
    const box = $('convo');
    const s = selectedSession(v);
    const m = s ? v.messages.get(s.id) : null;
    const restart = workspaceCanRun(v) || workspaceCanComment(v);
    const signature = JSON.stringify([s && s.id, s && sessionText(s), m && [m.list.length, m.error, m.loaded], restart]);
    if (signature === v.drawn.convo) return;
    const fresh = !v.drawn.convo || JSON.parse(v.drawn.convo)[0] !== (s && s.id);
    v.drawn.convo = signature;
    const atEnd = box.scrollHeight - box.scrollTop - box.clientHeight < 48;
    if (!s) {
      fill(box, h('section', { class: 'empty convo-empty' },
        h('span', { class: 'empty-mark', 'aria-hidden': 'true' }, icon('plus')),
        h('h2', { text: 'No sessions yet' }),
        h('p', { text: workspaceCanRun(v) || workspaceCanComment(v)
          ? 'A session is one agent working on this workspace\'s sources. Start one with + Session; what it says reads here, and the files it delivers wait under Outputs.'
          : 'Sessions that members start appear here, with what each agent said and delivered.' })));
      return;
    }
    const working = sessionActive(s);
    const phase = sessionPhase(s);
    const turns = conversation((m ? m.list : []).map((x) => ({
      activity: x.role === 'user' ? { prompt: x.text } : { harness: { type: 'message_delta', text: x.text } },
    })));
    const workingLine = () => h('p', { class: 'outcome' }, dot('running'), sessionStatus(s) ? 'Working…' : 'Starting…');
    fill(box,
      m && m.error ? h('p', { class: 'error', text: m.error }) : null,
      turns.map((t, i) => h('article', { class: 'turn' },
        t.text != null ? h('div', { class: 'you' }, h('p', { text: t.text })) : null,
        t.said.length ? agentBlock(t.said, s.harness) : null,
        working && i === turns.length - 1 ? workingLine() : null)),
      working && !turns.length ? workingLine() : null,
      RECOVERABLE_PHASES.includes(phase.key) ? h('section', { class: 'recovery', 'aria-label': `${s.name}: ${phase.label}` },
        h('p', { class: 'recovery-head' }, dot(phase.lane), h('span', { text: sessionText(s) })),
        h('p', { class: 'recovery-next', text: phase.next }),
        restart ? h('button', { type: 'button', id: 'try-again', text: 'Try again', onclick: () => tryAgain(v, s) }) : null) : null,
      !working && !unstarted(s) && !turns.length && m && m.loaded ? h('p', { class: 'muted', text: 'No messages yet.' }) : null);
    if (atEnd || fresh) box.scrollTop = box.scrollHeight;
  }

  function applySessionComposer(v) {
    const s = selectedSession(v);
    const { mode, why } = sessionComposer(v, s);
    const area = $('composer-text');
    $('composer').hidden = v.composing;
    $('composer').dataset.mode = why ? 'off' : mode.action;
    $('composer-label').textContent = mode.title;
    area.placeholder = why ? sentence(why, s && s.name) : mode.placeholder(s);
    area.disabled = !!why;
    $('composer-go').disabled = !!why || v.sending;
    $('composer-go').textContent = mode.label;
    $('composer-allow-box').hidden = true;
    $('composer-cancel').hidden = true;
  }

  async function submitSession(v) {
    if (wsView !== v) return;
    const s = selectedSession(v);
    const { mode, why } = sessionComposer(v, s);
    const area = $('composer-text');
    if (why || v.sending) return;
    const text = area.value.trim() || mode.empty;
    if (!text) { area.focus(); return; }
    v.sending = true;
    applySessionComposer(v);
    const verb = mode.label.toLowerCase();
    let op = null;
    try {
      op = await revised(v, `Could not ${verb}`, () => api('POST', `${workspacePath(v.id)}/sessions/${enc(s.id)}/messages`, { prompt: text, action: mode.action }));
    } catch (e) {
      toast(`Could not ${verb}: ${e.message}`);
    }
    v.sending = false;
    if (wsView !== v) return;
    if (op) {
      area.value = '';
      toast(mode.started);
      follow(v, op, mode.label);
    }
    applySessionComposer(v);
  }

  async function cancelSession(v, s) {
    if (!(await confirmDialog('Cancel turn?', `Cancel ${s.name}'s running turn?`, 'Cancel turn', true)) || wsView !== v) return;
    let op = null;
    try {
      op = await revised(v, 'Could not cancel the turn', () => api('POST', `${workspacePath(v.id)}/sessions/${enc(s.id)}/cancel`, {}));
    } catch (e) {
      toast(`Could not cancel the turn: ${e.message}`);
    }
    if (!op || wsView !== v) return;
    toast('Cancelling the running turn.');
    follow(v, op, 'Cancel', 'Turn cancelled.');
  }

  const LIFECYCLE = {
    archive: { busy: 'Archiving…', finished: 'Archived. Its sources, transcripts and accepted outputs are kept.' },
    restore: { busy: 'Restoring…', finished: 'Restored.' },
    sleep: { busy: 'Going to sleep…', finished: 'Asleep.' },
  };

  async function lifecycle(v, action) {
    const ws = v.workspace;
    if (action === 'archive' && !(await confirmDialog(`Archive ${ws.title}?`,
      'Its sources, transcripts and accepted outputs are kept, and Restore brings it back. No session runs while it is archived.', 'Archive'))) return;
    if (wsView !== v) return;
    v.busy = LIFECYCLE[action].busy;
    drawStrip(v);
    let op = null;
    try {
      op = await revised(v, `Could not ${action} ${ws.title}`, (revision) => api('POST', `${workspacePath(v.id)}/actions/${action}`, { revision }));
    } catch (e) {
      const reason = e.detail && e.detail.reason ? ` (${e.detail.reason})` : '';
      toast(`Could not ${action} ${ws.title}: ${e.message}${reason}.`);
    }
    v.busy = null;
    if (wsView !== v) return;
    if (op) follow(v, op, cap(action), LIFECYCLE[action].finished);
    drawWorkspace(v);
  }

  // Sources upload one at a time: each upload moves the revision the next
  // one must name. A file the server refuses is named and the rest still
  // go; a stale revision or a failed reload stops, naming what was not sent.
  async function uploadSources(v, files) {
    const sent = [];
    const refused = [];
    let unsent = [];
    for (const [i, file] of files.entries()) {
      let source = null;
      const title = sourceTitle(file.name);
      try {
        source = await revised(v, `Could not upload ${file.name}`, (revision) => call(`${workspacePath(v.id)}/sources`, {
          method: 'POST',
          headers: {
            Accept: 'application/json',
            'Content-Type': file.type || 'application/octet-stream',
            'X-Source-Title-Encoded': encodedTitle(title),
            'X-Source-Title': asciiTitle(title),
            'X-Workspace-Revision': String(revision),
            'Idempotency-Key': key(),
          },
          body: file,
        }));
      } catch (e) {
        if (wsView !== v) return;
        refused.push(`${file.name} (${e.message})`);
        continue;
      }
      if (wsView !== v) return;
      if (!source) { unsent = files.slice(i); break; }
      sent.push(file.name);
      try { await reloadDetail(v); } catch (_) { unsent = files.slice(i + 1); break; }
    }
    if (wsView !== v) return;
    if (sent.length) toast(`Uploaded ${plural(sent.length, 'source')}.`);
    if (refused.length) toast(`Not uploaded: ${refused.join('; ')}.`);
    if (unsent.length) toast(`Not sent: ${unsent.map((f) => f.name).join(', ')}. Upload ${unsent.length === 1 ? 'it' : 'them'} again.`);
    drawWorkspace(v);
  }

  // Refusals that mean the page's list was out of date: what happened, and
  // what to do once the outputs are read again.
  const ACCEPT_REFUSED = {
    deliverable_not_current: ['an output you chose is no longer its session\'s current result', 'The outputs are reloaded; accept the ones marked Current.'],
    producer_busy: ['the session that made these outputs is working on a newer turn', 'The outputs are reloaded; accept the ones marked Current once that turn finishes.'],
  };

  async function acceptSelected(v) {
    const ids = Array.from(v.selected);
    const names = ids.map((id) => ((v.artifacts || []).find((a) => a.id === id) || { name: id }).name);
    if (!(await confirmDialog(`Accept ${plural(ids.length, 'output')}?`,
      `${names.join(', ')} become accepted deliverables of ${v.workspace.title}. The server checks each came from its session's latest finished turn.`, 'Accept'))) return;
    if (wsView !== v) return;
    v.busy = 'Accepting…';
    drawStrip(v);
    let result = null;
    try {
      result = await revised(v, 'Could not accept', (revision) => api('POST', `${workspacePath(v.id)}/finalize`, { revision, artifacts: ids }));
    } catch (e) {
      const refused = e.status === 409 && ACCEPT_REFUSED[e.code];
      if (refused) {
        let reloaded = true;
        try { await reloadDetail(v); } catch (_) { reloaded = false; }
        if (wsView === v) scheduleWorkspace(v);
        toast(`Not accepted: ${refused[0]}. ${reloaded ? refused[1] : 'Reload the page, then try again.'}`);
      } else {
        toast(`Could not accept: ${e.message}`);
      }
    }
    v.busy = null;
    if (wsView !== v) return;
    if (result) {
      v.selected.clear();
      toast(`Accepted ${plural(result.artifacts.length, 'output')}.`);
      try { await reloadDetail(v); } catch (_) { /* the poll catches up */ }
    }
    drawWorkspace(v);
  }

  // A source's or output's name: it opens that file in the preview.
  function previewButton(v, item) {
    return h('button', {
      type: 'button', class: 'file-name', 'data-preview': item.key, 'aria-pressed': String(v.previewing === item.key),
      title: `Preview ${item.name}`, text: item.name, onclick: () => openPreview(v, item),
    });
  }

  function drawSources(v) {
    const sources = sourcesOf(v);
    $('sources-count').textContent = sources.length ? String(sources.length) : '';
    const signature = JSON.stringify(sources.map((src) => src.id));
    if (signature === v.drawn.sources) return;
    v.drawn.sources = signature;
    fill($('sources'), sources.length ? sources.map((src) => {
      const base = `${workspacePath(v.id)}/sources/${enc(src.id)}`;
      return h('li', { class: 'file-item', 'data-source': src.id },
        h('div', { class: 'file-main' },
          previewButton(v, { key: `source:${src.id}`, name: src.title, mediaType: src.media_type, base }),
          h('span', { class: 'item-meta', title: src.media_type }, `${formatOf(src.media_type, src.title)} · ${size(src.size_bytes)} · `, ago(src.created_at_ms))),
        h('button', { type: 'button', class: 'ghost', text: 'Download', 'aria-label': `Download ${src.title}`, onclick: () => download(`${base}/content`, src.title) }));
    })
      : h('li', { class: 'muted pad', text: 'No sources yet. Upload the documents this workspace\'s sessions should read; each new session gets the ones you tick.' }));
  }

  // Where an output stands toward acceptance. The server accepts an output
  // only from its session's current turn, once that turn finished with its
  // result and nothing more is queued for the session (`finalize` in
  // console/sources.rs); `current` is exactly that.
  function standing(a, s, accepted) {
    if (accepted.has(a.id)) return 'accepted';
    if (s && a.turn < s.turns) return 'earlier';
    if (s && sessionActive(s)) return 'working';
    const info = s && sessionStatus(s);
    const finished = info && ['ready', 'no_changes', 'merged'].includes(info.status.state) && s.result_event != null && !s.result_error;
    return finished ? 'current' : 'unsettled';
  }

  const STANDING = {
    accepted: { pick: false, tag: 'Accepted', note: (a) => `turn ${a.turn}` },
    current: { pick: true, tag: 'Current', note: (a) => `turn ${a.turn}` },
    working: { pick: false, note: (a) => `turn ${a.turn} · a newer turn is running` },
    earlier: { pick: false, note: (a) => `Earlier turn ${a.turn} — not the current result` },
    unsettled: { pick: false, note: (a) => `turn ${a.turn} · this turn ended without its result` },
  };

  // Outputs under the session that made them, in session order; within a
  // session, the newest turn first.
  function outputGroups(v) {
    const accepted = new Set((v.workspace.accepted_deliverables || []).map((d) => d.artifact_id));
    const sessions = sessionsOf(v);
    const byBranch = new Map(sessions.map((s) => [s.branch, s]));
    const groups = new Map();
    for (const a of v.artifacts || []) {
      const s = byBranch.get(a.publisher_branch) || null;
      if (!groups.has(a.publisher_branch)) groups.set(a.publisher_branch, { s, name: s ? s.name : a.publisher_branch, rows: [] });
      groups.get(a.publisher_branch).rows.push({ a, standing: standing(a, s, accepted) });
    }
    const order = (g) => (g.s ? sessions.indexOf(g.s) : sessions.length);
    return Array.from(groups.values()).sort((x, y) => order(x) - order(y))
      .map((g) => ({ ...g, rows: g.rows.sort((x, y) => y.a.turn - x.a.turn) }));
  }

  // The outputs a reviewer can be given: each session's latest, named as
  // listed, with its session and turn when two share a name.
  function reviewable(v) {
    const rows = outputGroups(v).flatMap((g) => g.rows.filter((r) => r.standing !== 'earlier').map((r) => ({ ...r.a, from: g.name })));
    return rows.map((a) => ({ ...a, label: rows.filter((b) => b.name === a.name).length > 1 ? `${a.name} (${a.from}, turn ${a.turn})` : a.name }));
  }

  // Ticked outputs that can no longer be accepted are unticked.
  function prune(v) {
    if (!v.artifacts) return;
    const pickable = new Set(outputGroups(v).flatMap((g) => g.rows).filter((r) => STANDING[r.standing].pick).map((r) => r.a.id));
    for (const id of v.selected) if (!pickable.has(id)) v.selected.delete(id);
  }

  function drawOutputs(v) {
    const list = $('outputs-list');
    if (!list || wsView !== v) return;
    const all = v.artifacts || [];
    const groups = outputGroups(v);
    $('outputs-count').textContent = all.length ? String(all.length) : '';
    const signature = JSON.stringify([groups.map((g) => [g.name, g.s && sessionText(g.s), g.rows.map((r) => [r.a.id, r.standing])]),
      v.artifactsError, v.artifacts === null, workspaceCanRun(v)]);
    if (signature === v.drawn.outputs) return;
    v.drawn.outputs = signature;
    const row = ({ a, standing: st }) => {
      const meaning = STANDING[st];
      const note = nextId('output-note');
      const box = h('input', {
        type: 'checkbox', 'aria-label': `Select ${a.name}`, 'aria-describedby': note,
        checked: st === 'accepted' || v.selected.has(a.id), disabled: !meaning.pick || !workspaceCanRun(v),
      });
      box.addEventListener('change', () => {
        if (box.checked) v.selected.add(a.id); else v.selected.delete(a.id);
        drawStrip(v);
        drawGuide(v);
      });
      return h('li', { class: 'output-row', 'data-artifact': a.id, 'data-standing': st },
        box,
        h('div', { class: 'file-main' },
          previewButton(v, { key: `output:${a.id}`, name: a.name, mediaType: a.media_type, base: `${workspacePath(v.id)}/artifacts/${enc(a.id)}` }),
          h('span', { class: 'item-meta', id: note, title: a.media_type, text: `${formatOf(a.media_type, a.name)} · ${size(a.size)} · ${meaning.note(a)}` })),
        meaning.tag ? h('span', { class: `tag tag-${st}` }, st === 'accepted' ? checkMark(false) : null, meaning.tag) : null);
    };
    const group = (g) => {
      const heading = nextId('output-group');
      return h('section', { class: 'output-group', 'aria-labelledby': heading },
        h('h3', { id: heading }, g.s ? dot(sessionLane(g.s)) : null, h('span', { class: 'group-name', text: g.name }),
          g.s ? h('span', { class: 'group-meta', text: cap(sessionText(g.s)) }) : null),
        h('ul', { class: 'file-list', 'aria-labelledby': heading }, g.rows.map(row)));
    };
    fill(list,
      v.artifactsError ? h('p', { class: 'error pad', text: `Could not list outputs: ${v.artifactsError}` }) : null,
      v.artifacts === null ? h('p', { class: 'muted pad', text: 'Loading outputs…' })
        : !all.length ? h('p', { class: 'muted pad', text: `No outputs yet. A session started with Collect outputs publishes the files it lists in ${OUTPUT_MANIFEST} when its turn ends.` })
          : groups.map(group));
  }

  // Mount the preview component for one source or output in the open
  // panel. A table, a page, or why the server made neither is the
  // component's to show; the original stays one click away.
  function openPreview(v, item) {
    closePreview();
    v.previewing = item.key;
    for (const b of document.querySelectorAll('#inspector button[data-preview]')) b.setAttribute('aria-pressed', String(b.dataset.preview === item.key));
    const slot = $('preview');
    $(`panel-${v.tab}`).append(slot);
    const host = h('div', { class: 'preview-host', id: 'preview-host' });
    const frameSlot = h('div', { class: 'frame-slot', id: 'frame-slot' });
    // An HTML output can also be run: Open puts it in an iframe that has
    // its own origin and no network (see openFrame).
    const runnable = item.key.startsWith('output:') && /\.html?$/i.test(item.name);
    fill(slot,
      h('div', { class: 'preview-head' },
        h('h3', { id: 'preview-title', text: item.name, title: item.name }),
        runnable ? h('button', { type: 'button', text: 'Open', 'aria-label': `Open ${item.name}`, onclick: (e) => openFrame(item, frameSlot, e.currentTarget) }) : null,
        h('button', { type: 'button', text: 'Download original', onclick: () => download(`${item.base}/content`, item.name) })),
      host, frameSlot);
    livePreview = window.BranchyardArtifactPreview.mount(host, { baseUrl: item.base, mediaType: item.mediaType, fetchAuthenticated: authFetch });
  }

  // Run an HTML output in a sandboxed iframe. The server mints a short-lived
  // address for it (a frame cannot send the bearer token) and serves that
  // path with a policy of its own; `sandbox="allow-scripts"` without
  // `allow-same-origin` gives the page an opaque origin, so it reaches
  // neither this page's storage nor its token.
  async function openFrame(item, slot, button) {
    if (slot.firstChild) { fill(slot); button.textContent = 'Open'; button.setAttribute('aria-label', `Open ${item.name}`); return; }
    button.disabled = true;
    try {
      const { path } = await call(`${item.base}/frame`, { method: 'POST' });
      const frame = h('iframe', { class: 'output-frame', title: `Running ${item.name}`, sandbox: 'allow-scripts', referrerpolicy: 'no-referrer', src: path });
      fill(slot, frame);
      button.textContent = 'Close';
      button.setAttribute('aria-label', `Close ${item.name}`);
    } catch (e) {
      toast(`Could not open ${item.name}: ${e.message}`);
    } finally {
      button.disabled = false;
    }
  }

  function drawDetails(v) {
    const ws = v.workspace;
    const project = projectOf(ws.project_id);
    const sessions = sessionsOf(v);
    const caps = ws.capabilities || {};
    const signature = JSON.stringify([ws.state, ws.revision, project && project.name, sessions.map((s) => [s.id, sessionText(s)]), ws.members, caps]);
    if (signature === v.drawn.details) return;
    v.drawn.details = signature;
    const lane = workspaceLane(ws);
    const unavailable = Object.entries(caps).filter(([, c]) => c && c.available === false);
    const rows = [
      ['Project', project ? h('a', { href: `#/p/${enc(project.id)}`, text: project.name }) : ws.project_id],
      ['State', h('span', { class: 'fact-state' }, dot(lane.key), cap(workspaceStateText(ws)))],
      ['Revision', String(ws.revision)],
      ['Results', cap(resultText(ws.result_mode))],
      ['Task', h('span', { class: 'mono', text: ws.task_id })],
      ['Created', new Date(ws.created_at_ms).toLocaleString()],
      ...unavailable.map(([name, c]) => [cap(name), `not available: ${c.reason}`]),
    ];
    fill($('ws-facts'), ...rows.map(([k, val]) => [h('dt', { text: k }), h('dd', {}, val)]).flat());
    fill($('ws-sessions'), sessions.length ? sessions.map((s) => h('li', {},
      h('a', { href: workspaceHref(v.id, s.id), text: s.name }),
      ` · ${[s.harness, s.access, sessionText(s)].filter(Boolean).join(' · ')}`))
      : h('li', { class: 'muted', text: 'None yet.' }));
    const members = ws.members || [];
    fill($('ws-members'), members.length ? members.map((m) => h('li', {}, h('span', { class: 'mono', text: m.subject }), ` · ${m.role}`))
      : h('li', { class: 'muted', text: 'This server lists no members.' }));
  }

  // An environment's or a harness's readiness, one row each in words: the
  // state, where it was measured, how long ago, the server's reason and
  // what an operator can do. The dot only repeats the state.
  function readinessList(rows, id, at) {
    return h('ul', { class: 'readiness', id }, rows.map((r) => {
      if (!r.state) {
        return h('li', { class: 'readiness-row' }, h('span', { class: 'readiness-mark', 'aria-hidden': 'true' }),
          h('span', {}, h('b', { text: `${r.name}: ` }), `${r.fact}.`));
      }
      const [word, lane] = READY[r.state] || READY.unknown;
      return h('li', { class: 'readiness-row', 'data-state': r.state }, dot(lane), h('span', {},
        h('b', { text: `${r.name}: ` }), `${word}, ${SCOPE[r.scope] || r.scope}`,
        r.checkedMsAgo == null ? null : [', checked ', ago(at - r.checkedMsAgo)], '. ', r.reason,
        r.action ? [' ', h('span', { class: 'readiness-action', text: `Operator: ${r.action}` })] : null));
    }));
  }

  // The guided form for a new session, in the conversation pane until it
  // starts or is cancelled. It is drawn from `v.draft`, which every control
  // writes, so leaving it and coming back loses nothing. A poll redraws only
  // the parts whose content changed and puts focus back where it was; the
  // name and the prompt are never redrawn.
  function openSessionForm(v) {
    const box = $('convo');
    if (!box || box.querySelector('#session-form')) return;
    if (!v.draft) v.draft = freshDraft(v);
    const choices = () => startChoices(v.environments, { run: workspaceCanRun(v), comment: workspaceCanComment(v) }, v.draft, reviewable(v));
    const blocked = (c) => (notReady(v.workspace) ? { field: 'workspace', text: notReady(v.workspace) } : c.missing);
    const heading = h('h2', { id: 'session-form-h', tabindex: '-1', text: 'New session' });
    const sourcesSlot = h('div', { class: 'slot', id: 'sf-sources' });
    const envSlot = h('div', { class: 'slot', id: 'sf-environments' });
    const agentSlot = h('div', { class: 'slot', id: 'sf-agent' });
    const collectSlot = h('div', { class: 'slot', id: 'sf-collect-slot' });
    const advancedSlot = h('div', { class: 'slot', id: 'sf-advanced-slot' });
    const advanced = h('details', { class: 'advanced', id: 'sf-advanced' }, h('summary', { id: 'sf-advanced-summary', text: 'Advanced' }), advancedSlot);
    const name = h('input', { id: 'sf-name', type: 'text', autocomplete: 'off', placeholder: 'For example: Analyst' });
    name.value = v.draft.name;
    name.addEventListener('input', () => { v.draft.name = name.value; syncForm(); });
    const prompt = h('textarea', { id: 'sf-prompt', rows: '5', placeholder: 'What should this session do?', 'aria-describedby': 'sf-prompt-hint' });
    prompt.value = v.draft.prompt;
    prompt.addEventListener('input', () => { v.draft.prompt = prompt.value; syncForm(); });
    const start = h('button', { type: 'button', class: 'primary', id: 'sf-start', text: 'Start session' });
    const status = h('p', { class: 'form-status', id: 'sf-status', role: 'status' });
    const error = h('p', { class: 'error form-error', id: 'sf-error', role: 'alert' });
    const cancel = h('button', {
      type: 'button', id: 'sf-cancel', text: 'Cancel',
      onclick: () => {
        v.composing = false;
        v.drawn.convo = null;
        drawWorkspace(v);
        const area = $('composer-text');
        (area.disabled ? $('new-session') || $('workspace').querySelector('h1') : area).focus();
      },
    });
    const toggle = (id, label, checked, change, describedby) => {
      const input = h('input', { type: 'checkbox', id, checked, 'aria-describedby': describedby });
      input.addEventListener('change', () => change(input.checked));
      return h('label', { class: 'toggle', for: id }, input, h('span', { text: label }));
    };
    const field = (id, label, control) => [h('label', { class: 'block', for: id, text: label }), control];
    const reasons = (id, lines) => (lines.length ? h('p', { class: 'hint reasons', id }, lines.map((text) => h('span', { text }))) : null);

    const drawSources = () => {
      const sources = sourcesOf(v);
      return [
        sources.length
          ? h('div', { class: 'choices' }, sources.map((src) => toggle(`sf-source-${src.id}`, src.title, v.draft.sources.get(src.id) !== false,
            (on) => { v.draft.sources.set(src.id, on); syncForm(); })))
          : h('p', { class: 'hint', text: 'No sources yet. Upload the files it should read, such as a spreadsheet, a document or a CSV.' }),
        workspaceCanRun(v) ? h('div', { class: 'row step-tools' },
          h('button', { type: 'button', id: 'sf-upload', text: 'Upload files', onclick: () => $('source-file').click() })) : null,
      ];
    };

    const environmentCard = (env, why, checked) => {
      const id = `sf-env-${env.key}`;
      const radio = h('input', {
        type: 'radio', name: 'environment', id, value: env.key, checked, disabled: !!why,
        'aria-describedby': `${id}-provider ${id}-about ${id}-ready`,
      });
      radio.addEventListener('change', () => { if (radio.checked) { v.draft.env = env.key; syncForm(); } });
      const card = h('div', { class: 'env-card', 'data-environment': env.key },
        radio,
        h('div', { class: 'env-body' },
          h('div', { class: 'env-head' }, h('label', { class: 'env-name', for: id, text: env.label }),
            h('span', { class: 'tag', id: `${id}-provider`, text: PROVIDERS[env.provider] || env.provider })),
          h('p', { class: 'env-about', id: `${id}-about` }, env.description, why ? h('span', { class: 'env-why', text: ` ${why}` }) : null),
          readinessList(env.readiness, `${id}-ready`, v.environments.at)));
      card.addEventListener('click', (ev) => { if (!radio.disabled && !ev.target.closest('label, input')) radio.click(); });
      return card;
    };
    const drawEnvironments = (c) => {
      const loaded = v.environments.state;
      if (c.envs.length) return h('div', { class: 'env-list' }, c.envs.map(({ env, why }) => environmentCard(env, why, env === c.env)));
      if (loaded === 'failed') {
        return [h('p', { class: 'error', text: `The environments did not load: ${said(v.environments.error)}` }),
          h('div', { class: 'row step-tools' }, h('button', { type: 'button', id: 'sf-retry', text: 'Retry', onclick: () => loadEnvironments(v) }))];
      }
      return h('p', { class: 'hint', text: loaded === 'loading' ? 'Loading the environments this server offers…' : 'This server offers no environment for this workspace; an operator can add one.' });
    };

    const drawAgent = (c) => {
      if (!c.env) {
        return h('p', { class: 'hint', text: v.environments.state === 'ready' ? 'Choose where it runs; each environment offers its own harnesses.' : 'The harnesses follow once the environments load.' });
      }
      let harness;
      const closed = c.harnesses.filter((x) => x.why);
      if (c.typed) {
        harness = h('input', { id: 'sf-harness', type: 'text', autocomplete: 'off', spellcheck: 'false', placeholder: 'A harness this server runs', 'aria-describedby': 'sf-harness-why' });
        harness.value = v.draft.harness || '';
        harness.addEventListener('input', () => { v.draft.harness = harness.value; syncForm(); });
      } else {
        const describedby = [closed.length ? 'sf-harness-why' : null, c.harness ? 'sf-harness-ready' : null].filter(Boolean).join(' ');
        harness = h('select', { id: 'sf-harness', 'aria-describedby': describedby || null },
          c.harness ? null : h('option', { value: '', disabled: true, text: 'Choose a harness' }),
          c.harnesses.map(({ harness: x, why }) => h('option', { value: x.id, disabled: !!why, text: why ? `${x.id} (not available: ${why})` : x.id })));
        harness.value = c.harness ? c.harness.id : '';
        harness.addEventListener('change', () => { v.draft.harness = harness.value; v.draft.ack = false; syncForm(); });
      }
      let model = null;
      if (c.model.kind === 'select') {
        const select = h('select', { id: 'sf-model' }, c.model.options.map((m) => h('option', { value: m, text: m === c.harness.defaultModel ? `${m} (default)` : m })));
        select.value = c.model.value;
        select.addEventListener('change', () => { v.draft.model = select.value; syncForm(); });
        model = field('sf-model', 'Model', select);
      } else if (c.model.kind === 'free') {
        const input = h('input', { id: 'sf-model', type: 'text', autocomplete: 'off', placeholder: 'Harness default' });
        input.value = v.draft.model;
        input.addEventListener('input', () => { v.draft.model = input.value; syncForm(); });
        model = field('sf-model', 'Model (optional)', input);
      } else if (c.harness) {
        model = [h('p', { class: 'field-title', text: 'Model' }), h('p', { class: 'hint', id: 'sf-model-none', text: 'This environment chooses the model.' })];
      }
      return [
        h('div', { class: 'agent-grid' },
          h('div', {}, field('sf-harness', 'Harness', harness),
            c.typed ? reasons('sf-harness-why', ['This environment lists no harness; type one this server runs.'])
              : reasons('sf-harness-why', closed.map(({ harness: x, why }) => `${x.id} is not available: ${why}.`))),
          h('div', {}, model)),
        c.harness && c.harness.readiness.length ? readinessList(c.harness.readiness, 'sf-harness-ready', v.environments.at) : null,
        c.acknowledgement ? h('div', { class: 'ack' },
          toggle('sf-ack', 'I accept tools without per-tool approval', v.draft.ack, (on) => { v.draft.ack = on; syncForm(); }, 'sf-ack-why'),
          h('p', { class: 'hint', id: 'sf-ack-why', text: `${c.harness.id} cannot ask before each tool it uses, so as a writer it runs them without asking. Tick this only if you accept that.` })) : null,
      ];
    };

    const drawCollect = (c) => (c.access.value !== 'writer' ? null : [
      toggle('sf-collect', `Collect outputs (${OUTPUT_MANIFEST})`, v.draft.collect, (on) => { v.draft.collect = on; syncForm(); }, 'sf-collect-hint'),
      h('p', { class: 'hint', id: 'sf-collect-hint', text: `Collect outputs asks the session to list its result files in ${OUTPUT_MANIFEST} and end with a JSON summary; the server publishes those files as outputs you can preview and accept.` }),
    ]);

    const drawAdvanced = (c) => {
      const closed = [['writer', 'Writer'], ['reviewer', 'Reviewer']].filter(([k]) => c.access[k]).map(([k, label]) => `${label} is not available: ${c.access[k]}.`);
      const access = h('select', { id: 'sf-access', disabled: c.access.fixed, 'aria-describedby': closed.length ? 'sf-access-why' : null },
        h('option', { value: 'writer', disabled: !!c.access.writer && c.access.value !== 'writer', text: 'Writer: works in its own copy' }),
        h('option', { value: 'reviewer', disabled: !!c.access.reviewer && c.access.value !== 'reviewer', text: 'Reviewer: reads outputs you choose' }));
      access.value = c.access.value;
      access.addEventListener('change', () => { v.draft.access = access.value; syncForm(); });
      const outputs = reviewable(v);
      const writer = c.access.value === 'writer';
      let effort = null;
      if (c.effort.offered) {
        effort = h('select', { id: 'sf-effort' }, h('option', { value: '', text: 'Harness default' }), EFFORTS.map((e) => h('option', { value: e, text: e })));
        effort.value = c.effort.value;
        effort.addEventListener('change', () => { v.draft.effort = effort.value; syncForm(); });
      }
      return [
        field('sf-access', 'Access', access),
        reasons('sf-access-why', closed),
        writer ? null : h('fieldset', { class: 'choices', id: 'sf-outputs' }, h('legend', { text: 'Outputs to review' }),
          outputs.length ? outputs.map((a) => toggle(`sf-output-${a.id}`, a.label, v.draft.outputs.has(a.id), (on) => {
            if (on) v.draft.outputs.add(a.id); else v.draft.outputs.delete(a.id);
            syncForm();
          })) : h('p', { class: 'hint', text: 'No outputs yet: a reviewer needs at least one.' })),
        effort ? field('sf-effort', 'Effort', effort) : null,
        writer ? h('div', { class: 'row choices-row' }, toggle('sf-allow', 'Allow every tool', v.draft.allowAll, (on) => { v.draft.allowAll = on; syncForm(); }, 'sf-tools-hint')) : null,
        h('p', { class: 'hint', id: 'sf-tools-hint', text: 'A writer asks before each tool it uses unless you allow every tool (like --yes). A harness that cannot ask starts as a writer only with the acknowledgement under Agent, and only where this server permits it. Reviewers stay read-only.' }),
      ];
    };

    const readyText = (c) => {
      const reads = sourcesOf(v).filter((src) => v.draft.sources.get(src.id) !== false).length;
      const reviews = reviewable(v).filter((a) => v.draft.outputs.has(a.id)).length;
      return `Ready to start: ${[c.env.label, c.harness.id, `reads ${plural(reads, 'source')}`, c.access.value === 'reviewer' ? `reviews ${plural(reviews, 'output')}` : null].filter(Boolean).join(' · ')}`;
    };
    const FOCUS = {
      workspace: () => $('strip').querySelector('details.menu > summary'),
      environments: () => $('sf-retry'),
      environment: () => envSlot.querySelector('input[type="radio"]:not(:disabled)'),
      harness: () => $('sf-harness'),
      ack: () => $('sf-ack'),
      name: () => name,
      prompt: () => prompt,
      outputs: () => {
        advanced.open = true;
        return advancedSlot.querySelector('#sf-outputs input:not(:disabled)') || $('sf-access');
      },
    };
    const focusField = (key) => {
      const el = FOCUS[key] && FOCUS[key]();
      if (el) el.focus();
    };

    // Each part keeps the signature it was drawn from; one that changed is
    // drawn again and gets back the focus it held, by id.
    const drawnParts = new Map();
    const redraw = (slot, signature, draw) => {
      if (drawnParts.get(slot) === signature) return;
      drawnParts.set(slot, signature);
      const focused = slot.contains(document.activeElement) ? document.activeElement.id : null;
      fill(slot, draw());
      if (!focused) return;
      const back = $(focused) || slot.querySelector('input:not(:disabled), select:not(:disabled), button:not(:disabled)') || heading;
      back.focus({ preventScroll: true });
    };
    let submitting = false;
    let forcedShown = false;
    let settling = true;
    const syncForm = () => {
      if (!heading.isConnected) return;
      const c = choices();
      const harnesses = c.harnesses.map(({ harness: x, why }) => [x.id, why]);
      redraw(sourcesSlot, JSON.stringify([sourcesOf(v).map((src) => [src.id, src.title]), workspaceCanRun(v)]), drawSources);
      redraw(envSlot, JSON.stringify([v.environments.state, v.environments.error, v.environments.at, c.envs.map(({ env, why }) => [env.key, why])]), () => drawEnvironments(c));
      redraw(agentSlot, JSON.stringify([v.environments.at, c.env && c.env.key, c.typed, harnesses, c.typed || !c.harness ? null : c.harness.id,
        c.model.kind, c.model.options, c.acknowledgement]), () => drawAgent(c));
      redraw(collectSlot, c.access.value, () => drawCollect(c));
      redraw(advancedSlot, JSON.stringify([c.access, reviewable(v).map((a) => [a.id, a.label]), c.effort.offered]), () => drawAdvanced(c));
      if (c.access.forced && !forcedShown) {
        advanced.open = true;
        forcedShown = true;
      }
      const missing = blocked(c);
      const text = missing ? missing.text : readyText(c);
      if (status.textContent !== text) status.textContent = text;
      start.disabled = submitting || v.environments.state !== 'ready';
      const label = submitting ? 'Starting…' : 'Start session';
      if (start.textContent !== label) start.textContent = label;
      // Once the environments are in, focus left on the heading moves to
      // what the start needs first.
      if (settling && v.environments.state === 'ready') {
        settling = false;
        if (document.activeElement !== heading) return;
        if (!missing) start.focus();
        else if (missing.field !== 'workspace') focusField(missing.field);
      }
    };

    const refuse = async (e) => {
      const known = START_REFUSED[e.code];
      if (known && known.reload === 'workspace') {
        try { await reloadDetail(v); } catch (_) { /* the refusal is still said */ }
        if (wsView === v) drawWorkspace(v);
      }
      if (known && known.reload === 'environments') await loadEnvironments(v);
      if (wsView !== v) return;
      error.textContent = known ? known.text(e, v) : `${said(e.message)} Your draft is kept.`;
    };
    start.addEventListener('click', async () => {
      if (submitting) return;
      const c = choices();
      const missing = blocked(c);
      error.textContent = '';
      if (missing) {
        status.textContent = missing.text;
        focusField(missing.field);
        return;
      }
      const body = startRequest(c, v.draft, sourcesOf(v), reviewable(v));
      submitting = true;
      syncForm();
      let op = null;
      let refusal = null;
      try {
        op = await revised(v, 'Could not start the session', () => api('POST', `${workspacePath(v.id)}/sessions`, body));
      } catch (e) {
        refusal = e;
      }
      submitting = false;
      if (wsView !== v) return;
      if (refusal) await refuse(refusal);
      if (!op) {
        if (wsView === v && v.composing) syncForm();
        return;
      }
      v.sent.set(op.session_id, body);
      v.draft = freshDraft(v, { env: c.env.key, harness: c.harness.id, model: c.model.kind === 'none' ? v.draft.model : c.model.value });
      toast('Session started.');
      v.composing = false;
      v.drawn.convo = null;
      try { await reloadDetail(v); } catch (_) { /* the poll catches up */ }
      follow(v, op, 'Session');
      if (wsView === v) go(workspaceHref(v.id, op.session_id));
    });

    fill(box, h('section', { class: 'card form session-form', id: 'session-form', 'aria-labelledby': 'session-form-h' },
      heading,
      h('fieldset', { class: 'step' }, h('legend', { text: 'Sources it reads' }), sourcesSlot),
      h('fieldset', { class: 'step' }, h('legend', { text: 'Where it runs' }), envSlot),
      h('fieldset', { class: 'step' }, h('legend', { text: 'Agent' }), agentSlot),
      h('fieldset', { class: 'step' }, h('legend', { text: 'Task' }),
        field('sf-name', 'Session name', name),
        field('sf-prompt', 'First prompt', prompt),
        h('p', { class: 'hint', id: 'sf-prompt-hint', text: 'Say what it should do and which files it should produce, for example: Summarise the backlog by owner in an XLSX and a one-page DOCX.' }),
        collectSlot),
      advanced,
      h('div', { class: 'row form-actions' }, start, cancel),
      status,
      error));
    v.syncForm = syncForm;
    heading.focus({ preventScroll: true });
    syncForm();
  }

  // ---------------------------------------------------------------
  // A new task

  async function renderNew() {
    if (!state.repo) {
      main(h('div', { class: 'page narrow' }, h('a', { class: 'back', href: '#/', text: '‹ Back' }),
        h('h1', { text: 'New task' }),
        h('p', { class: 'lede', text: 'A task runs in a repository, and this token reaches none. Start a knowledge project instead.' }),
        can('run') ? h('div', { class: 'row form-actions' }, h('a', { class: 'button primary', href: '#/p/new', text: 'New project' })) : null));
      return;
    }
    const promptId = nextId('prompt');
    const nameId = nextId('name');
    const harnessId = nextId('harness');
    const allowId = nextId('allow');
    const area = h('textarea', { id: promptId, required: true, rows: '6' });
    const nameInput = h('input', { id: nameId, type: 'text', autocomplete: 'off', spellcheck: 'false' });
    const select = h('select', { id: harnessId }, h('option', { value: '', text: 'the server\'s default' }));
    const allow = h('input', { type: 'checkbox', id: allowId });
    const error = h('p', { class: 'error', role: 'alert' });
    const go = h('button', {
      type: 'button', class: 'primary', text: 'Start task', disabled: !can('run'),
      onclick: async () => {
        const prompt = area.value.trim();
        if (!prompt) { area.focus(); return; }
        const body = { prompt };
        if (select.value) body.harness = select.value;
        if (nameInput.value.trim()) body.name = nameInput.value.trim();
        if (allow.checked) body.policy = { mode: 'allow' };
        go.disabled = true;
        try {
          const op = await api('POST', `${repoPath()}/tasks`, body);
          toast('Task started. Its branch is on the list.');
          followOperation(op.id, { label: 'Task' }, op.branches[0] || '');
          location.hash = op.branches.length ? branchHref(op.branches[0]) : '#/';
        } catch (e) {
          error.textContent = e.message;
          go.disabled = false;
        }
      },
    });
    main(h('div', { class: 'page narrow' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'New task' }),
        h('p', { class: 'lede', text: `It runs in ${state.repo}, on a branch of its own.` }))),
      h('div', { class: 'card form' },
        h('label', { class: 'block', for: promptId, text: 'Prompt' }), area,
        h('div', { class: 'form-grid' },
          h('div', {}, h('label', { class: 'block', for: harnessId, text: 'Harness' }), select),
          h('div', {}, h('label', { class: 'block', for: nameId, text: 'Branch name (optional)' }), nameInput)),
        h('label', { class: 'toggle', for: allowId }, allow, h('span', { text: 'Allow every tool (like --yes); otherwise tool requests are denied' })),
        h('div', { class: 'row form-actions' }, go),
        can('run') ? null : h('p', { class: 'hint', text: 'This token lacks the run scope.' })),
      error));
    try {
      const { harnesses } = await api('GET', '/v1/harnesses');
      for (const hn of harnesses.filter((x) => x.available)) select.append(h('option', { value: hn.harness, text: hn.harness }));
    } catch (_) { /* the default harness still works */ }
  }

  // ---------------------------------------------------------------
  // Needs you: approvals a turn waits on, questions and escalations
  // waiting for an answer, branches that stopped for a person, the
  // permission requests seen while this page was open, and the effect
  // ledger

  let decisionsTimer = null;
  function refreshDecisions() {
    clearTimeout(decisionsTimer);
    decisionsTimer = setTimeout(loadDecisions, 300);
  }

  // What waits for a person, in the server's order: a branch parked on an
  // approval or a question first, oldest wait first, then the ready
  // branches, then the stopped and stalled ones. The questions among them
  // are also the inbox the lanes read.
  async function loadDecisions() {
    let decisions;
    if (!state.repo) return;
    try { ({ decisions } = await api('GET', `${repoPath()}/decisions`)); } catch (_) { return; }
    // The ground costs the server a diff per open branch: read it only for
    // a page that shows it, a merge card or a branch.
    if (decisions.some((d) => d.kind === 'merge') || state.route.name === 'branch') await loadGround();
    state.decisions = decisions;
    state.inbox = decisions.filter((d) => d.kind === 'question').map((d) => d.question);
    branchesChanged();
  }

  // Which files are in whose hands: every path an open branch changed,
  // with the branches that changed it. A failed read keeps the last one.
  async function loadGround() {
    const repo = state.repo;
    if (!repo) return;
    let ground;
    try { ground = await api('GET', `${repoPath()}/ground`); } catch (_) { return; }
    if (repo === state.repo) state.ground = { repo, files: ground.files || [], skipped: ground.skipped || [] };
  }

  // `name`'s changed files with the other open branches holding each:
  // [{ path, others }], or null before the ground is read.
  function groundOf(name) {
    const g = state.ground;
    if (!g || g.repo !== state.repo) return null;
    return g.files.filter((f) => f.branches.some((b) => b.name === name))
      .map((f) => ({ path: f.path, others: f.branches.map((b) => b.name).filter((n) => n !== name) }));
  }

  let approvalsTimer = null;
  function refreshApprovals() {
    clearTimeout(approvalsTimer);
    approvalsTimer = setTimeout(loadApprovals, 300);
  }

  async function loadApprovals() {
    try {
      // Every approval, so the deck can say what was answered today; the
      // ones still open are what the lanes and the deck count.
      const [{ approvals }, { effects }] = await Promise.all([
        api('GET', `${repoPath()}/approvals?all=true`),
        api('GET', `${repoPath()}/effects`),
      ]);
      state.approvals = approvals.filter((x) => !x.answer);
      state.answered = approvals.filter((x) => x.answer).sort((x, y) => y.answer.at_ms - x.answer.at_ms).slice(0, 50);
      state.effects = effects.slice(-50).reverse();
    } catch (_) { return; }
    await loadDecisions();
  }

  function aboutText(about) {
    switch (about.kind) {
      case 'tool': return `use the tool ${about.tool}`;
      case 'operation': return `${about.connector} ${about.operation} (${about.class}${about.deletion ? ', deletes' : ''})`;
      case 'promote': return `perform the staged ${about.connector} ${about.operation} (${about.class})`;
      default: return about.kind;
    }
  }

  function answerForm(m) {
    const id = nextId('answer');
    const area = h('textarea', { id, rows: '2', 'data-draft': `answer:${m.id}`, placeholder: m.kind === 'escalation' ? 'Optional note' : 'Your answer (Enter sends, Shift+Enter adds a line)' });
    const disabled = !can('run');
    const send = async (prefix) => {
      const note = area.value.trim();
      const text = prefix ? (note ? `${prefix} ${note}` : prefix) : note;
      if (!text) { area.focus(); return; }
      try {
        await api('POST', `${branchPath(m.to)}/answer`, { message_id: m.id, text });
        toast(`Answered ${m.from}.`);
        loadApprovals();
      } catch (e) {
        toast(`Could not answer: ${e.message}`);
      }
    };
    // Enter sends a question's answer, as it does in a chat box; an
    // escalation has two answers, so its Enter stays a new line.
    if (m.kind !== 'escalation') {
      area.addEventListener('keydown', (ev) => {
        if (ev.key !== 'Enter' || ev.shiftKey || ev.isComposing || ev.metaKey || ev.ctrlKey || ev.altKey || disabled) return;
        ev.preventDefault();
        send(null);
      });
    }
    return h('div', { class: 'reply' },
      h('label', { class: 'block', for: id, text: `Answer as ${m.to}` }), area,
      h('div', { class: 'row' },
        m.kind === 'escalation'
          ? [h('button', { type: 'button', class: 'primary', disabled, text: 'Approve', onclick: () => send('Approved.') }),
            h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => send('Denied.') })]
          : h('button', { type: 'button', class: 'primary', disabled, text: 'Answer', onclick: () => send(null) })),
      disabled ? h('p', { class: 'hint', text: 'This token lacks the run scope.' }) : null);
  }

  function approvalForm(a) {
    const id = nextId('approval');
    const area = h('textarea', { id, rows: '2', 'data-draft': `approval:${a.id}`, placeholder: 'Optional reason' });
    const disabled = !can('run');
    const send = async (allow) => {
      if (!allow || a.about.kind === 'promote') {
        const ok = await confirmDialog(allow ? 'Perform it?' : 'Deny it?',
          allow ? `This performs ${aboutText(a.about)} for real.` : `${a.branch} is refused ${aboutText(a.about)}.`,
          allow ? 'Perform' : 'Deny', !allow);
        if (!ok) return;
      }
      const reason = area.value.trim();
      try {
        await api('POST', `${repoPath()}/approvals/${enc(a.id)}/${allow ? 'allow' : 'deny'}`,
          { surface: 'companion', ...(reason ? { reason } : {}) });
        toast(`${allow ? 'Allowed' : 'Denied'}: ${a.branch} may${allow ? '' : ' not'} ${aboutText(a.about)}.`);
        loadApprovals();
      } catch (e) {
        toast(`Could not answer the approval: ${e.message}`);
      }
    };
    return h('div', { class: 'reply' },
      h('label', { class: 'block', for: id, text: 'Reason' }), area,
      h('div', { class: 'row' },
        h('button', { type: 'button', class: 'primary', disabled, text: 'Allow', onclick: () => send(true) }),
        h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => send(false) })),
      disabled ? h('p', { class: 'hint', text: 'This token lacks the run scope.' }) : null);
  }

  function attentionSignature() {
    const needs = lanes(Array.from(state.branches.values()))[0].branches;
    const d = state.deck;
    return JSON.stringify([state.decisions.map(decisionKey), d.later, d.front, d.holding.map((x) => x.key),
      Array.from(d.evidence.entries()).map(([k, e]) => [k, e.loading, e.said && e.said.length, e.files && e.files.length]),
      projectCards().map(decisionKey), state.workspaces.list.map((w) => w.title),
      state.approvals.map((a) => a.id), state.inbox.map((m) => m.id), state.permissions,
      state.effects.map((e) => [e.updated_ms, e.state]), needs.map((b) => [b.name, b.status, b.stalled]),
      state.ground && state.ground.files, state.answered.map((a) => a.id),
      Array.from(state.tracks.values()).map((t) => t.track && t.track.merged), Math.floor(Date.now() / 30000)]);
  }

  // How long an allow or a deny waits before it is sent, for a second
  // thought. The agent stays parked meanwhile.
  const UNDO_MS = 8000;
  const HOLD_MS = 700;
  const decisionKey = (d) => `${d.kind}:${d.branch}:${d.approval ? d.approval.id : d.question ? d.question.id : d.output ? d.output.id : ''}`;
  const WAITS = { agent: 'an agent is parked until you answer', delivery: 'delivery waits on you', nobody: 'nothing waits on this' };
  const KIND_LABEL = { approval: 'Approval', question: 'Question', merge: 'Ready to merge', stop: 'Stopped', stall: 'Silent', output: 'Delivered', session: 'Stopped' };
  const KIND_VERB = { approval: 'allow or deny', question: 'answer', merge: 'merge', stop: 'resume', stall: 'look', output: 'accept', session: 'open' };
  // Who a card is about: a branch, or a workspace's session.
  const subjectOf = (d) => (d.session_name || d.branch);
  // What the bay and the line under a card say of a decision.
  function summaryOf(d) {
    switch (d.kind) {
      case 'approval': return `${d.branch} wants to ${aboutText(d.approval.about)}`;
      case 'question': return `${d.branch} asks: ${d.question.text}`;
      case 'merge': return `${d.branch} is ready to merge`;
      case 'stall': return `${d.branch} is silent`;
      case 'output': return `${d.session_name} delivered ${d.output.name}`;
      case 'session': return `${d.session_name}: ${d.phase.toLowerCase()}`;
      default: return `${d.branch} ${statusOf({ status: d.status }).text}`;
    }
  }

  // The decisions in the server's order: a parked agent first, then what
  // waits on delivery, then what nobody waits on; the repository's before
  // the project's within each.
  const WAIT_RANK = { agent: 0, delivery: 1, nobody: 2 };
  function allDecisions() {
    return state.decisions.concat(projectCards())
      .map((d, i) => [d, i]).sort((x, y) => WAIT_RANK[x[0].waits] - WAIT_RANK[y[0].waits] || x[1] - y[1]).map(([d]) => d);
  }

  // The deck in the order to take it: the server's order, with the cards
  // sent to the back at the back, the one brought forward in front, and
  // the answers held for their undo window left out.
  function deckOrder() {
    const d = state.deck;
    const held = new Set(d.holding.map((x) => x.key));
    const cards = allDecisions().filter((x) => !held.has(decisionKey(x)));
    const later = cards.filter((x) => d.later.includes(decisionKey(x))).sort((x, y) => d.later.indexOf(decisionKey(x)) - d.later.indexOf(decisionKey(y)));
    const now = cards.filter((x) => !d.later.includes(decisionKey(x)));
    const ordered = now.concat(later);
    const front = ordered.findIndex((x) => decisionKey(x) === d.front);
    if (front > 0) ordered.unshift(ordered.splice(front, 1)[0]);
    return ordered;
  }

  // Send a card to the back of the deck.
  function later(d) {
    const key = decisionKey(d);
    state.deck.later = state.deck.later.filter((k) => k !== key).concat(key);
    if (state.deck.front === key) state.deck.front = null;
    branchesChanged();
    focusDeck();
  }

  function focusDeck() {
    const card = document.querySelector('#main .deck-card');
    if (card) card.focus({ preventScroll: true });
  }

  // Hold an answer for its undo window, then send it. Undo within the
  // window and nothing was sent.
  function holdAnswer(d, allow, reason) {
    const entry = { key: decisionKey(d), decision: d, allow, reason, until: Date.now() + UNDO_MS, timer: null };
    entry.timer = setTimeout(() => sendAnswer(entry), UNDO_MS);
    state.deck.holding.push(entry);
    branchesChanged();
    focusDeck();
  }

  async function sendAnswer(entry) {
    state.deck.holding = state.deck.holding.filter((x) => x !== entry);
    const a = entry.decision.approval;
    try {
      await api('POST', `${repoPath()}/approvals/${enc(a.id)}/${entry.allow ? 'allow' : 'deny'}`,
        { surface: 'companion', ...(entry.reason ? { reason: entry.reason } : {}) });
      toast(`${entry.allow ? 'Allowed' : 'Denied'}: ${a.branch} may${entry.allow ? '' : ' not'} ${aboutText(a.about)}.`);
    } catch (e) {
      toast(`Could not answer the approval: ${e.message}`);
    }
    loadApprovals();
  }

  function undoAnswer(entry) {
    clearTimeout(entry.timer);
    state.deck.holding = state.deck.holding.filter((x) => x !== entry);
    branchesChanged();
    focusDeck();
  }

  // The evidence a merge or a stop card shows: the branch's last turn (what
  // it said and did) and its changed files, fetched once per branch and
  // kept until the branch's status changes.
  function evidenceFor(d) {
    const key = `${d.branch}@${JSON.stringify(d.status)}`;
    const found = state.deck.evidence.get(key);
    if (found) return found;
    const entry = { loading: true, said: [], tools: [], files: [], error: null };
    state.deck.evidence.set(key, entry);
    (async () => {
      try {
        const page = await api('GET', `${branchPath(d.branch)}/event-page?limit=${EVENT_WINDOW}`);
        const turns = conversation(page.events);
        const lastTurn = turns[turns.length - 1];
        if (lastTurn) { entry.said = lastTurn.said; entry.tools = lastTurn.tools; }
        if (d.candidate) {
          const diff = await api('GET', `${branchPath(d.branch)}/diff`);
          entry.files = parseDiff(diff.diff || '');
        }
      } catch (e) { entry.error = e.message; }
      entry.loading = false;
      branchesChanged();
    })();
    return entry;
  }

  // The one preview a deck card mounts, kept across the redraws of the
  // card (its table or page is not fetched again) and disposed when
  // another card comes to the front or the page is left.
  let deckPreview = null;
  function closeDeckPreview() {
    if (deckPreview) deckPreview.handle.dispose();
    deckPreview = null;
  }
  function deckPreviewHost(d, base) {
    const key = decisionKey(d);
    if (!deckPreview || deckPreview.key !== key) {
      closeDeckPreview();
      const host = h('div', { class: 'preview-host', id: 'preview-host' });
      deckPreview = { key, host, handle: window.BranchyardArtifactPreview.mount(host, { baseUrl: base, mediaType: d.output.media_type, fetchAuthenticated: authFetch }) };
    }
    return deckPreview.host;
  }

  // Accept an output where the workspace page does: the workspace's current
  // revision with the output's id. A refusal says why and the cards reload.
  async function acceptOutput(d) {
    const name = d.output.name;
    try {
      const ws = await api('GET', workspacePath(d.workspace));
      await api('POST', `${workspacePath(d.workspace)}/finalize`, { revision: ws.revision, artifacts: [d.output.id] });
      toast(`Accepted ${name}.`);
    } catch (e) {
      toast(e.code === 'deliverable_not_current' ? `Not accepted: ${name} is no longer its session's current result.` : `Could not accept ${name}: ${e.message}`);
    }
    await loadProjectWork(state.projectDecisions.project, state.workspaces.list);
    drawn = null;
    branchesChanged();
    focusDeck();
  }

  // Resume an interrupted session as the workspace page's message box does.
  async function resumeSession(d) {
    try {
      await api('POST', `${workspacePath(d.workspace)}/sessions/${enc(d.session)}/messages`, { prompt: RESUME_PROMPT, action: 'resume' });
      toast(`Resuming ${d.session_name}. A new turn is starting.`);
    } catch (e) {
      toast(`Could not resume ${d.session_name}: ${e.message}`);
    }
    await loadProjectWork(state.projectDecisions.project, state.workspaces.list);
    drawn = null;
    branchesChanged();
  }

  // What a stopped session last said, fetched once per state it stopped in.
  function sessionEvidence(d) {
    const key = `${d.session}@${d.phase}`;
    const found = state.deck.evidence.get(key);
    if (found) return found;
    // A session that never started has no log to read.
    const entry = { loading: !d.phase.startsWith('Did not start'), said: null, error: null };
    state.deck.evidence.set(key, entry);
    if (!entry.loading) return entry;
    (async () => {
      try {
        // The log is read a page at a time from its cursor, as the
        // workspace page reads it; the last line is on the last page.
        const messages = [];
        for (let cursor = '0'; ;) {
          const page = await api('GET', `${workspacePath(d.workspace)}/sessions/${enc(d.session)}/messages?after=${enc(cursor)}`);
          messages.push(...page.messages);
          if (page.next_cursor === cursor) break;
          cursor = page.next_cursor;
        }
        const turns = conversation(messages.map((m) => ({
          activity: m.role === 'user' ? { prompt: m.text } : { harness: { type: 'message_delta', text: m.text } },
        })));
        const said = turns.length ? turns[turns.length - 1].said : [];
        entry.said = said.length ? said[said.length - 1] : null;
      } catch (e) { entry.error = e.message; }
      entry.loading = false;
      branchesChanged();
    })();
    return entry;
  }

  // Hold to act: the fill is the confirmation. Under reduced motion, two
  // presses: the first arms, the second fires.
  function lever(label, run) {
    const button = h('button', { type: 'button', class: 'primary lever' }, h('span', { text: label }), h('kbd', { text: '↵' }));
    const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
    let timer = null;
    let fired = false;
    const fire = () => { fired = true; button.classList.remove('hold'); button.dataset.fired = ''; button.disabled = true; run(); };
    const start = () => { if (fired || timer) return; button.classList.add('hold'); timer = setTimeout(() => { timer = null; fire(); }, HOLD_MS); };
    const cancel = () => { if (timer) { clearTimeout(timer); timer = null; } if (!fired) button.classList.remove('hold'); };
    if (reduced) {
      button.addEventListener('click', () => {
        if (fired) return;
        if (button.dataset.armed) fire();
        else { button.dataset.armed = '1'; button.firstChild.textContent = `${label} now`; setTimeout(() => { if (!fired) { delete button.dataset.armed; button.firstChild.textContent = label; } }, 4000); }
      });
    } else {
      button.addEventListener('pointerdown', (ev) => { ev.preventDefault(); start(); });
      for (const name of ['pointerup', 'pointerleave', 'pointercancel']) button.addEventListener(name, cancel);
      button.addEventListener('keydown', (ev) => { if ((ev.key === 'Enter' || ev.key === ' ') && !ev.repeat) { ev.preventDefault(); start(); } });
      button.addEventListener('keyup', (ev) => { if (ev.key === 'Enter' || ev.key === ' ') cancel(); });
      button.addEventListener('blur', cancel);
    }
    return button;
  }

  const layerText = (resolved) => (!resolved ? null : resolved.layer === 'admin' ? "the administrator's policy asks before this" : resolved.layer === 'person' ? 'your own policy asks before this' : `the ${resolved.layer} policy asks before this`);

  // One decision as a card: the question at the top, the evidence in the
  // middle, the answers at the bottom, where the eye ends. Keys answer.
  function deckCard(d, position, total, rest) {
    const b = state.branches.get(d.branch) || { name: d.branch, status: d.status, candidate: d.candidate };
    const disabled = !can('run');
    const draftKey = `deck:${decisionKey(d)}`;
    let title;
    let body = [];
    let actions = [];
    let primary = null;
    const open = () => { location.hash = d.workspace ? workspaceHref(d.workspace, d.session) : branchHref(d.branch); };
    const followUp = () => { location.hash = d.workspace ? workspaceHref(d.workspace, d.session) : branchHref(d.branch); setTimeout(() => { const area = $('composer-text'); if (area) area.focus(); }, 0); };
    const since = d.since_ms ? h('span', { text: `parked on you for ${since_(d.since_ms)}` }) : null;
    if (d.kind === 'approval') {
      const a = d.approval;
      const reason = h('textarea', { rows: '1', 'data-draft': draftKey, placeholder: 'A reason, if you want one' });
      const evidence = evidenceFor(d);
      title = [d.branch, ' wants to ', h('code', { text: aboutText(a.about) })];
      const input = a.request && typeof a.request === 'object' && !Array.isArray(a.request) ? Object.entries(a.request).slice(0, 4) : [];
      body = [
        h('div', { class: 'deck-grid' },
          h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'What it was doing' }),
            evidence.loading ? skeleton() : evidence.said.length ? h('div', { class: 'rich' }, rich(evidence.said[evidence.said.length - 1])) : h('p', { class: 'muted', text: 'It said nothing before it asked.' })),
          h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'What it asks' }),
            h('dl', { class: 'deck-facts' },
              h('dt', { text: a.about.kind === 'tool' ? 'Tool' : 'Call' }), h('dd', {}, h('code', { text: a.about.kind === 'tool' ? a.about.tool : aboutText(a.about) })),
              input.map(([k, v]) => [h('dt', { text: k }), h('dd', {}, h('code', { text: (typeof v === 'string' ? v : JSON.stringify(v)).slice(0, 160) }))]),
              h('dt', { text: 'Who asks' }), h('dd', { text: layerText(a.resolved) ? cap(layerText(a.resolved)) : 'The policy asks before this' }),
              h('dt', { text: 'Parked' }), h('dd', { text: `${d.since_ms ? `at ${clock(d.since_ms).slice(0, 5)}` : 'now'} · turn ${a.turn}` })))),
        a.request ? h('details', { class: 'request' }, h('summary', { text: 'The whole call' }), h('pre', { class: 'code', text: JSON.stringify(a.request, null, 2).slice(0, 2000) })) : null,
        h('label', { class: 'block deck-reason' }, h('span', { class: 'visually-hidden', text: 'Reason' }), reason),
      ];
      primary = () => holdAnswer(d, true, reason.value.trim());
      actions = [
        h('button', { type: 'button', class: 'primary', disabled, text: 'Allow', onclick: primary }, h('kbd', { text: '↵' })),
        h('button', { type: 'button', class: 'danger', disabled, text: 'Deny', onclick: () => holdAnswer(d, false, reason.value.trim()) }, h('kbd', { text: 'n' })),
      ];
    } else if (d.kind === 'question') {
      const m = d.question;
      title = [m.from, m.kind === 'escalation' ? ' escalates: ' : ' asks: ', m.text];
      body = [h('p', { class: 'deck-line' }, since, h('span', { class: 'muted', text: ` · to ${m.to}` })), answerForm(m)];
    } else if (d.kind === 'merge') {
      const c = d.candidate;
      const evidence = evidenceFor(d);
      title = [d.branch, ' is ready to merge'];
      const stats = [c ? h('span', { class: 'mono', text: c.commit.slice(0, 7) }) : null, c ? [`${plural(c.files_changed, 'file')} `, counts(c.insertions, c.deletions)] : null,
        b.turns != null ? plural(b.turns, 'turn') : null, b.cost_usd != null ? money(b.cost_usd) : null, b.harness || null].filter(Boolean);
      const did = evidence.tools.length ? toolCounts(evidence.tools) : null;
      const shared = (groundOf(d.branch) || []).filter((f) => f.others.length);
      body = [
        h('p', { class: 'deck-line deck-stats' }, stats.map((x) => h('span', { class: 'stat-chip' }, x))),
        shared.length ? h('p', { class: 'deck-line deck-ground' }, h('b', { text: 'In two hands: ' }),
          shared.map((f, i) => [i ? '; ' : '', h('code', { text: f.path }), ` also in ${f.others.join(', ')}`])) : null,
        b.prompt ? h('p', { class: 'deck-asked' }, h('span', { class: 'muted', text: 'Asked: ' }), b.prompt) : null,
        h('div', { class: 'deck-grid' },
          h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'What it says' }),
            evidence.loading ? skeleton() : evidence.said.length ? h('div', { class: 'rich' }, rich(evidence.said[evidence.said.length - 1])) : h('p', { class: 'muted', text: evidence.error || 'It said nothing in its last turn.' })),
          h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'What it did' }),
            evidence.loading ? skeleton() : did ? h('ul', { class: 'deck-did' },
              did.run ? h('li', {}, icon('terminal', true), `ran ${plural(did.run, 'command')}`) : null,
              did.edit ? h('li', {}, icon('pencil', true), `edited ${plural(did.edit, 'time')}`) : null,
              did.read ? h('li', {}, icon('eye', true), `read ${plural(did.read, 'time')}`) : null,
              did.other ? h('li', {}, icon('wrench', true), `${plural(did.other, 'other call')}`) : null,
              h('li', { class: 'muted' }, icon('bolt', true), 'the harness reports what ran, not whether it passed'))
              : h('p', { class: 'muted', text: 'No tool calls in its last turn.' }))),
        evidence.files.length ? h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'Changes' }),
          h('ul', { class: 'deck-files' }, evidence.files.map((f) => {
            const at = f.path.lastIndexOf('/');
            return h('li', {}, h('span', { class: 'path' }, h('span', { class: 'dir', text: f.path.slice(0, at + 1) }), h('b', { text: f.path.slice(at + 1) })),
              h('span', { class: 'file-counts' }, counts(f.add, f.del)), fileBar(f.add, f.del));
          })),
          h('pre', { class: 'diff deck-diff', tabindex: '0', 'aria-label': `The first lines of ${evidence.files[0].path}` },
            evidence.files[0].lines.slice(0, 14).map((l) => h('span', { class: l.kind }, l.text)),
            h('span', { class: 'file', text: '  … the rest is in Work' }))) : null,
      ];
      actions = applies(ACTIONS.merge, b) ? [lever(`Merge ${c ? c.commit.slice(0, 7) : d.branch}`, () => { state.pending.set(d.branch, ACTIONS.merge.busy); act(ACTIONS.merge, d.branch).then((r) => { if (!r || !r.id) state.pending.delete(d.branch); }); })] : [];
      actions.push(h('button', { type: 'button', text: 'Follow up', onclick: followUp }, h('kbd', { text: 'f' })));
    } else if (d.kind === 'stop' || d.kind === 'stall') {
      const evidence = evidenceFor(d);
      title = d.kind === 'stall' ? [d.branch, ' has been silent for its stall window'] : [d.branch, ' ', statusOf(b).text];
      body = [
        h('p', { class: 'deck-line' }, h('span', { class: 'muted', text: `last active ${ago(activeAt(b))}${b.turns != null ? ` · ${plural(b.turns, 'turn')}` : ''}${b.cost_usd != null ? ` · ${money(b.cost_usd)}` : ''}` })),
        evidence.said.length ? h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'The last thing it said' }), h('div', { class: 'rich' }, rich(evidence.said[evidence.said.length - 1]))) : null,
        evidence.tools.length ? h('p', { class: 'deck-line muted' }, 'Last calls: ', evidence.tools.slice(-3).map((name, i) => [i ? ', ' : '', h('code', { text: name })])) : null,
      ];
      if (applies(ACTIONS.resume, b)) { primary = () => runAction(ACTIONS.resume, d.branch); actions = [h('button', { type: 'button', class: 'primary', text: 'Resume', onclick: primary }, h('kbd', { text: '↵' }))]; }
      else actions = [h('button', { type: 'button', class: 'primary', text: 'Follow up', onclick: followUp }, h('kbd', { text: 'f' }))];
      if (d.kind === 'stall' && applies(ACTIONS.cancel, b)) actions.push(h('button', { type: 'button', class: 'danger', text: 'Cancel turn', onclick: () => runAction(ACTIONS.cancel, d.branch) }));
    } else if (d.kind === 'output') {
      const o = d.output;
      const base = `${workspacePath(d.workspace)}/artifacts/${enc(o.id)}`;
      title = `${d.session_name} delivered ${o.name}`;
      body = [
        h('p', { class: 'deck-line' }, h('span', { class: 'muted', text: `${d.workspace_title} · ${formatOf(o.media_type, o.name)} · ${size(o.size)} · turn ${o.turn} · delivered ${since_(d.since_ms)} ago` })),
        h('div', { class: 'deck-section' },
          h('div', { class: 'preview-head' },
            h('h3', { class: 'deck-file', title: o.name, text: o.name }),
            h('button', { type: 'button', text: 'Download original', onclick: () => download(`${base}/content`, o.name) })),
          deckPreviewHost(d, base)),
      ];
      if (can('run')) {
        actions = [lever(`Accept ${o.name}`, () => acceptOutput(d))];
      } else {
        actions = [h('button', { type: 'button', class: 'primary', disabled: true, text: 'Accept' })];
      }
    } else if (d.kind === 'session') {
      const evidence = sessionEvidence(d);
      const interrupted = d.status.state === 'interrupted' && !!d.phase && d.phase === 'Interrupted';
      title = d.status.state === 'awaiting_plan_approval' ? [d.session_name, `'s plan waits for your approval`] : [d.session_name, ` ${(d.phase.split(':')[0] || 'stopped').toLowerCase()}`];
      body = [
        h('p', { class: 'deck-line' }, h('span', { class: 'muted', text: `${d.workspace_title} · ${d.phase}` })),
        evidence.said ? h('div', { class: 'deck-section' }, h('span', { class: 'deck-label', text: 'The last thing it said' }), h('div', { class: 'rich' }, rich(evidence.said))) : null,
        evidence.loading ? skeleton() : null,
        !evidence.loading && !evidence.said ? h('p', { class: 'muted', text: evidence.error || 'It has said nothing.' }) : null,
      ];
      if (interrupted && can('run')) {
        primary = () => resumeSession(d);
        actions = [h('button', { type: 'button', class: 'primary', text: 'Resume', onclick: primary }, h('kbd', { text: '↵' }))];
      } else {
        primary = open;
        actions = [h('button', { type: 'button', class: 'primary', text: 'Open', onclick: open }, h('kbd', { text: '↵' }))];
      }
    }
    const card = h('article', { class: `deck-card deck-${d.kind}`, tabindex: '0', 'aria-label': `${KIND_LABEL[d.kind]}: ${subjectOf(d)}` },
      h('div', { class: 'deck-head' },
        h('span', { class: 'deck-head-row' },
          h('span', { class: `deck-kind deck-kind-${d.waits}` }, dot(d.waits === 'agent' ? 'needs' : d.kind === 'merge' || d.kind === 'output' ? 'ready' : 'done'),
            h('span', {}, h('b', { text: KIND_LABEL[d.kind] }), ` · ${WAITS[d.waits]}`)),
          d.since_ms ? h('span', { class: 'deck-clock', title: `since ${clock(d.since_ms)}` }, ago(d.since_ms, d.kind === 'output' ? ' ago' : ' waiting')) : null),
        h('h2', {}, title)),
      h('div', { class: 'deck-body' }, body),
      h('div', { class: 'deck-actions' }, actions,
        h('button', { type: 'button', class: 'ghost', text: 'Open', onclick: open }, h('kbd', { text: 'o' })),
        h('span', { class: 'spacer' }),
        h('button', { type: 'button', class: 'ghost', text: 'Later', onclick: () => later(d) }, h('kbd', { text: 's' }))));
    card.addEventListener('keydown', (ev) => {
      if (ev.metaKey || ev.ctrlKey || ev.altKey) return;
      if (ev.target.closest('input, textarea, select, button, a')) return;
      const key = ev.key;
      if (key === 'Enter' && primary && !ev.repeat) primary();
      else if (key === 'n' && d.kind === 'approval' && !disabled) holdAnswer(d, false, '');
      else if (key === 's') later(d);
      else if (key === 'o') open();
      else if (key === 'f') followUp();
      else return;
      ev.preventDefault();
    });
    return card;
  }
  const since_ = (ms) => since(ms).replace(/ ago$/, '');

  // The bay beside the card: the answers held for undo, then what is next.
  function deckBay(rest) {
    const held = state.deck.holding.map((entry) => {
      const a = entry.decision.approval;
      const left = h('span', { class: 'undo-left', 'data-until': String(entry.until) });
      return h('div', { class: `bay-item bay-held bay-${entry.allow ? 'allow' : 'deny'}` },
        h('span', { class: 'bay-main' }, h('b', { text: `${a.branch}: ${entry.allow ? 'allowing' : 'denying'} ${aboutText(a.about)}` }),
          h('span', { class: 'bay-meta', text: 'sent when the count ends; the agent stays parked meanwhile' })),
        h('button', { type: 'button', class: 'small', onclick: () => undoAnswer(entry) }, 'Undo ', left));
    });
    const next = rest.map((d) => h('button', {
      type: 'button', class: `bay-item bay-${d.kind}`, 'data-decision': decisionKey(d),
      onclick: () => { state.deck.front = decisionKey(d); state.deck.later = state.deck.later.filter((k) => k !== decisionKey(d)); branchesChanged(); focusDeck(); },
    }, h('span', { class: 'bay-main' },
      h('b', { text: summaryOf(d) }),
      h('span', { class: 'bay-meta', text: `${KIND_LABEL[d.kind].toLowerCase()} · ${d.since_ms ? (d.kind === 'output' ? `delivered ${since_(d.since_ms)} ago` : `parked ${since_(d.since_ms)}`) : WAITS[d.waits]}` })),
    h('span', { class: 'bay-verb mono', text: KIND_VERB[d.kind] })));
    return h('aside', { class: 'deck-bay', 'aria-label': 'Up next' },
      held.length || next.length ? h('p', { class: 'deck-label bay-title' }, h('span', { text: 'Up next' }), next.length ? h('span', { class: 'lane-count', text: String(next.length) }) : null) : null,
      held, next,
      next.length ? h('p', { class: 'bay-why muted', text: 'Parked agents first, the longest wait first; then what delivery waits on; then the rest. Later sends a card to the back.' }) : null);
  }

  // What was decided today, newest first: approvals answered (here, in a
  // terminal or by the API), branches merged and effects the ledger
  // recorded. Each says when, and links to its branch.
  function recentDecisions() {
    const midnight = new Date().setHours(0, 0, 0, 0);
    const items = [];
    for (const a of state.answered) {
      if (a.answer.at_ms < midnight) continue;
      items.push({ at: a.answer.at_ms, kind: a.answer.allow ? 'allowed' : 'denied', branch: a.branch,
        text: [h('a', { href: branchHref(a.branch), text: a.branch }), ` may${a.answer.allow ? '' : ' not'} ${aboutText(a.about)}`],
        note: [`${a.answer.by} · ${a.answer.surface}`, a.answer.reason ? ` · “${a.answer.reason}”` : ''] });
    }
    for (const b of state.branches.values()) {
      if (b.status.state !== 'merged') continue;
      const t = trackFor(b);
      const ms = (t && t.merged) || activeAt(b);
      if (ms < midnight) continue;
      items.push({ at: ms, kind: 'merged', branch: b.name,
        text: [h('a', { href: branchHref(b.name), text: b.name }), ` merged into ${b.status.target}`],
        note: [b.candidate ? [`${plural(b.candidate.files_changed, 'file')} `, counts(b.candidate.insertions, b.candidate.deletions)] : null, b.cost_usd != null ? ` · ${cents(b.cost_usd)}` : null] });
    }
    for (const e of state.effects) {
      if (e.updated_ms < midnight) continue;
      items.push({ at: e.updated_ms, kind: 'effect', branch: e.branch,
        text: [h('a', { href: branchHref(e.branch), text: e.branch }), ` ${e.connector} ${e.operation}: ${e.state}`], note: e.class });
    }
    return items.sort((x, y) => y.at - x.at);
  }
  const RECENT_ICON = { allowed: 'check', denied: 'cross', merged: 'merge', effect: 'bolt' };

  function recentList(items) {
    return h('ol', { class: 'recent' }, items.slice(0, 8).map((x) => h('li', { class: `recent-${x.kind}` },
      h('span', { class: 'recent-mark', 'aria-hidden': 'true' }, icon(RECENT_ICON[x.kind], true)),
      h('span', { class: 'recent-text' }, h('span', { class: 'recent-what' }, h('b', { text: cap(x.kind) }), ' · ', x.text), h('span', { class: 'recent-note' }, x.note)),
      h('time', { class: 'recent-at', datetime: new Date(x.at).toISOString(), title: new Date(x.at).toLocaleString(), text: clock(x.at).slice(0, 5) }))),
    items.length > 8 ? h('li', { class: 'recent-more muted', text: `${items.length - 8} more earlier today` }) : null);
  }

  function renderAttention() {
    // Keep what was being typed, and where, across a redraw.
    const drafts = new Map(Array.from(document.querySelectorAll('#main [data-draft]'), (el) => [el.dataset.draft, el.value]));
    const active = document.activeElement && document.activeElement.dataset ? document.activeElement : null;
    const focused = active && active.dataset.draft ? [active.dataset.draft, active.selectionStart, active.selectionEnd] : null;
    const perms = state.permissions;
    const project = state.source.kind === 'project' ? projectOf(state.source.id) : null;
    const scope = [state.repo, project && project.name].filter(Boolean).join(' and ');
    const deck = deckOrder();
    if (!deck.length || deck[0].kind !== 'output' || decisionKey(deck[0]) !== (deckPreview && deckPreview.key)) closeDeckPreview();
    const held = state.deck.holding.length;
    const count = deck.length;
    const recent = state.source.kind === 'repo' || state.repo ? recentDecisions() : [];
    const peeks = deck.slice(1, 3);
    main(h('div', { class: 'page wide decide' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Needs you' }),
        h('p', { class: 'lede', text: !scope ? 'This token reaches no repository and no project is listed. Choose one in the sidebar.'
          : count ? `${plural(count, 'decision')} ${count === 1 ? 'waits' : 'wait'} for you in ${scope}. One at a time, the parked agent first.` : held ? 'Nothing more waits for you.' : `Nothing in ${scope} waits for you.` }))),
      count || held ? h('div', { class: 'deck' },
        count ? h('div', { class: 'deck-main' },
          h('p', { class: 'deck-count muted' }, h('b', { text: `1 of ${count}` }), count > 1 ? ` · then ${subjectOf(deck[1])}` : ' · the last one'),
          h('div', { class: `deck-pile deck-pile-${peeks.length}` },
            deckCard(deck[0], 1, count, deck.slice(1)),
            peeks.map((x, i) => h('div', { class: `deck-peek deck-peek-${i + 1} peek-${x.kind}`, 'aria-hidden': 'true' },
              i === 0 ? [h('span', { class: 'peek-label', text: 'Next' }), h('span', { class: 'peek-text', text: summaryOf(x) })] : null))),
          h('p', { class: 'deck-keys muted' }, h('kbd', { text: '↵' }), ' the first action (held, for a merge) · ', h('kbd', { text: 'n' }), ' deny · ', h('kbd', { text: 's' }), ' later · ', h('kbd', { text: 'o' }), ' open · ', h('kbd', { text: 'f' }), ' follow up · an allow or a deny waits ', String(UNDO_MS / 1000), ' s for an undo'))
          : null,
        deckBay(deck.slice(1))) : null,
      count || held ? null : h('section', { class: 'empty' },
        h('span', { class: 'empty-mark ok', 'aria-hidden': 'true' }, checkMark(false)),
        h('h2', { text: 'Nothing to decide' }),
        h('p', { text: 'An approval, a question, a branch ready to merge, one that stopped, an output to accept: each comes here as one card, the parked agent first. Agents keep working meanwhile.' }),
        h('div', { class: 'row' }, h('a', { class: 'button', href: '#/', text: 'Watch the board' }))),
      h('section', { class: 'recent-box', 'aria-labelledby': 'recent-h' },
        h('h2', { id: 'recent-h' }, 'Decided today', recent.length ? h('span', { class: 'lane-count', text: String(recent.length) }) : null),
        recent.length ? recentList(recent) : h('p', { class: 'muted recent-none', text: 'Nothing yet today. What you allow, deny or merge is listed here with the time.' })),
      h('details', { class: 'more' },
        h('summary', {}, 'More', h('span', { class: 'muted', text: ` · ${plural(perms.length, 'permission request')} seen, ${plural(state.effects.length, 'effect')} on the ledger` })),
      h('details', { class: 'attention-group ledger' },
        h('summary', {}, h('h2', { text: `Permission requests (${perms.length})` })),
        h('p', { class: 'hint', text: 'A server decides each tool request at once by the policy its task or send was given; there is nothing to approve afterwards. Send again with "Allow every tool" to let a branch use what was denied.' }),
        perms.length
          ? h('ul', { class: 'events' }, perms.map((p) => h('li', {}, h('time', { text: clock(p.at_ms) }),
            h('a', { href: branchHref(p.branch), text: p.branch }), ` asked to use ${p.tool}${p.decision ? `: ${p.decision}` : ''}`)))
          : h('p', { class: 'muted', text: 'None seen since this page opened.' })),
      h('details', { class: 'attention-group ledger' },
        h('summary', {}, h('h2', { text: `Effect ledger (${state.effects.length})` })),
        h('p', { class: 'hint', text: 'What branches did outside the machine. by undo plans and performs inverses where the upstream allows.' }),
        state.effects.length
          ? h('ul', { class: 'events' }, state.effects.map((e) => h('li', {}, h('time', { text: clock(e.updated_ms) }),
            h('a', { href: branchHref(e.branch), text: e.branch }),
            ` ${e.connector} ${e.operation}: ${e.state} (${e.class})${e.undo ? ` · undo ${e.undo.operation}` : (e.undo_unavailable ? ` · no undo: ${e.undo_unavailable}` : '')}`)))
          : h('p', { class: 'muted', text: 'No effects recorded.' })))));
    for (const el of document.querySelectorAll('#main [data-draft]')) {
      if (drafts.has(el.dataset.draft)) el.value = drafts.get(el.dataset.draft);
      if (focused && el.dataset.draft === focused[0]) { el.focus({ preventScroll: true }); el.setSelectionRange(focused[1], focused[2]); }
    }
  }

  // ---------------------------------------------------------------
  // Triggers

  async function renderTriggers() {
    const head = h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Triggers' }),
      h('p', { class: 'lede', text: 'Tasks that start on a schedule or on an event.' })));
    main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Branches' }), head, h('p', { class: 'muted', text: 'Loading…' })));
    let triggers;
    try { ({ triggers } = await api('GET', '/v1/triggers')); } catch (e) {
      main(h('div', { class: 'page' }, h('a', { class: 'back', href: '#/', text: '‹ Branches' }), head, h('p', { class: 'error', text: e.message })));
      return;
    }
    const whenText = (w) => w.kind === 'cron' ? `cron ${w.expr}${w.timezone && w.timezone !== 'UTC' ? ` (${w.timezone})` : ''}`
      : w.kind === 'interval' ? `every ${w.seconds}s` : `on ${w.source} events`;
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      head,
      triggers.length ? h('ul', { class: 'list' }, triggers.map((t) => {
        const box = h('input', { type: 'checkbox', id: nextId('trigger'), checked: t.enabled, disabled: !can('run') });
        box.addEventListener('change', async () => {
          box.disabled = true;
          try {
            await api('POST', `/v1/triggers/${enc(t.id)}/${box.checked ? 'enable' : 'disable'}`, {});
            toast(`${t.name} ${box.checked ? 'enabled' : 'disabled'}.`);
            renderTriggers();
          } catch (e) {
            box.checked = !box.checked;
            toast(`Could not change ${t.name}: ${e.message}`);
            box.disabled = false;
          }
        });
        return h('li', { class: 'item' },
          h('div', { class: 'item-head' }, dot(t.enabled ? 'ready' : 'done'), h('span', { class: 'item-name', text: t.name }), h('span', { class: 'tag', text: t.enabled ? 'enabled' : 'disabled' })),
          h('div', { class: 'item-meta', text: [t.repo, whenText(t.when), t.next_due_ms ? `next ${new Date(t.next_due_ms).toLocaleString()}` : null].filter(Boolean).join(' · ') }),
          t.paused_reason ? h('div', { class: 'item-meta', text: t.paused_reason }) : null,
          h('label', { class: 'toggle', for: box.id }, box, h('span', { text: 'Enabled' })));
      })) : h('p', { class: 'muted', text: 'No triggers. by trigger add makes one.' })));
  }

  // ---------------------------------------------------------------
  // Queue and stats

  async function renderQueue() {
    const all = Array.from(state.branches.values());
    const cost = all.reduce((s, b) => s + (b.cost_usd || 0), 0);
    const turns = all.reduce((s, b) => s + (b.turns || 0), 0);
    const opsSlot = h('div', {}, h('p', { class: 'muted', text: 'Loading…' }));
    const stat = (label, value, lane) => h('div', { class: 'stat' }, h('span', { class: 'glance-label' }, lane ? dot(lane) : null, label), h('b', { text: value }));
    main(h('div', { class: 'page' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Queue and stats' }),
        h('p', { class: 'lede', text: state.repo ? `Work queued and done in ${state.repo}.` : 'Work queued and done in your repositories.' }))),
      h('div', { class: 'stats' },
        stat('branches', String(all.length)),
        lanes(all).map(({ lane, branches }) => stat(lane.label.toLowerCase(), String(branches.length), branches.length ? lane.key : 'done')),
        stat('turns', String(turns)),
        stat('cost reported', money(cost))),
      h('h2', { text: 'Queued and running operations' }), opsSlot));
    if (!state.repo) { fill(opsSlot, h('p', { class: 'muted', text: 'This token reaches no repository.' })); return; }
    try {
      const { operations } = await api('GET', `${repoPath()}/operations`);
      fill(opsSlot, operations.length
        ? h('ul', { class: 'list' }, operations.map(opItem))
        : h('p', { class: 'muted', text: 'Nothing queued or running.' }));
    } catch (e) {
      fill(opsSlot, h('p', { class: 'error', text: e.message }));
    }
  }

  // ---------------------------------------------------------------
  // Settings: who this is, notifications, signing out

  function b64ToBytes(text) {
    const pad = '='.repeat((4 - (text.length % 4)) % 4);
    const raw = atob((text + pad).replace(/-/g, '+').replace(/_/g, '/'));
    return Uint8Array.from(raw, (c) => c.charCodeAt(0));
  }

  async function pushRegistration() {
    if (!('serviceWorker' in navigator)) return null;
    return navigator.serviceWorker.register('sw.js', { scope: './' });
  }

  async function renderSettings() {
    const me = state.me;
    const status = h('p', { role: 'status' });
    const pushSlot = h('div', {}, h('p', { class: 'muted', text: 'Checking…' }));
    main(h('div', { class: 'page narrow' },
      h('a', { class: 'back', href: '#/', text: '‹ Branches' }),
      h('header', { class: 'page-head' }, h('div', {}, h('h1', { text: 'Settings' }))),
      h('section', { class: 'card', 'aria-label': 'This token' },
        h('dl', { class: 'facts' },
          h('dt', { text: 'Name' }), h('dd', { text: me.name }),
          h('dt', { text: 'Tenant' }), h('dd', { text: me.tenant }),
          h('dt', { text: 'Scopes' }), h('dd', { text: me.scopes.join(', ') || 'none' }),
          h('dt', { text: 'Repositories' }), h('dd', { text: (me.repos || state.repos).join(', ') }),
          h('dt', { text: 'Kind' }), h('dd', { text: me.kind === 'paired' ? 'paired from a link' : 'from the server\'s configuration' }),
          me.expires_at_ms ? [h('dt', { text: 'Expires' }), h('dd', { text: new Date(me.expires_at_ms).toLocaleString() })] : null)),
      h('h2', { text: 'Notifications' }), h('div', { class: 'card' }, pushSlot, status),
      h('h2', { text: 'Sign out' }),
      h('div', { class: 'card' },
        h('p', { class: 'hint', text: 'Forgets the token in this tab. To stop the token working everywhere, revoke it on the server: by serve token revoke NAME.' }),
        h('div', { class: 'row form-actions' }, h('button', { type: 'button', class: 'danger', text: 'Sign out', onclick: () => signOut('Signed out.') })))));
    let info;
    try { info = await api('GET', '/v1/app/push'); } catch (e) {
      fill(pushSlot, h('p', { class: 'error', text: e.message }));
      return;
    }
    state.pushInfo = info;
    const supported = 'serviceWorker' in navigator && 'PushManager' in window && window.isSecureContext;
    const pageNote = h('p', { class: 'hint', text: 'While this page is open, a branch that needs you moves to the top of the list and the tab\'s title counts them. When the tab is hidden, and the browser allows it, a system notification says why.' });
    if (!info.enabled || !supported) {
      fill(pushSlot, pageNote, h('p', { class: 'hint', text: !info.enabled
        ? 'This server does not send push notifications.'
        : 'This browser cannot receive push notifications here (it needs HTTPS, or localhost, and push support).' }));
      return;
    }
    let subscription = null;
    try {
      const reg = await pushRegistration();
      await navigator.serviceWorker.ready;
      subscription = reg ? await reg.pushManager.getSubscription() : null;
    } catch (_) { subscription = null; }
    const on = !!subscription && info.subscriptions.includes(subscription.endpoint);
    state.pushOn = on;
    const toggle = h('button', {
      type: 'button', class: on ? '' : 'primary', text: on ? 'Turn off on this device' : 'Notify this device',
      onclick: async () => {
        toggle.disabled = true;
        try {
          const reg = await pushRegistration();
          await navigator.serviceWorker.ready;
          if (on) {
            const sub = await reg.pushManager.getSubscription();
            if (sub) {
              await api('DELETE', '/v1/app/push/subscriptions', { endpoint: sub.endpoint });
              await sub.unsubscribe();
            }
            status.textContent = 'Push notifications are off on this device.';
          } else {
            const permission = await Notification.requestPermission();
            if (permission !== 'granted') throw new Error('notifications were not allowed');
            let sub = await reg.pushManager.getSubscription();
            if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: b64ToBytes(info.public_key) });
            const json = sub.toJSON();
            await api('POST', '/v1/app/push/subscriptions', { endpoint: json.endpoint, keys: json.keys });
            status.textContent = 'This device will be notified.';
          }
        } catch (e) {
          status.textContent = `Could not change notifications: ${e.message}`;
        }
        renderSettings();
      },
    });
    const test = h('button', {
      type: 'button', text: 'Send a test', disabled: !on,
      onclick: async () => {
        try {
          const r = await api('POST', '/v1/app/push/test', {});
          status.textContent = r.delivered ? 'Sent: it should arrive in a moment.' : `Not sent: ${(r.failures || []).join('; ') || 'no subscription'}`;
        } catch (e) { status.textContent = e.message; }
      },
    });
    fill(pushSlot, pageNote,
      h('p', { text: on ? 'This device gets push notifications for the repositories this token can read.' : 'Get a push notification on this device even when the page is closed.' }),
      h('div', { class: 'row' }, toggle, test));
  }

  // ---------------------------------------------------------------
  // Start

  function onKey(ev) {
    if (ev.defaultPrevented || ev.metaKey || ev.ctrlKey || ev.altKey || !state.me || $('confirm').open) return;
    if (ev.target.closest('input, textarea, select, [contenteditable]')) return;
    const search = $('branch-filter');
    if (ev.key === '/' && search.offsetParent) { ev.preventDefault(); search.focus(); search.select(); }
    else if (ev.key === 'j' || ev.key === 'k') { ev.preventDefault(); stepBranch(ev.key === 'j' ? 1 : -1); }
  }

  async function boot() {
    $('repo').addEventListener('change', (ev) => pickSource(ev.target.value));
    $('branch-filter').addEventListener('input', (ev) => {
      state.filter = ev.target.value;
      renderLanes();
    });
    document.addEventListener('keydown', onKey);
    document.addEventListener('click', (ev) => {
      for (const menu of document.querySelectorAll('details.menu[open]')) if (!menu.contains(ev.target)) menu.open = false;
    });
    document.addEventListener('keydown', (ev) => {
      if (ev.key !== 'Escape') return;
      for (const menu of document.querySelectorAll('details.menu[open]')) { menu.open = false; menu.querySelector('summary').focus(); }
    });
    setInterval(() => {
      for (const t of document.querySelectorAll('time[data-at]')) t.textContent = since(Number(t.dataset.at)) + t.dataset.suffix;
      for (const u of document.querySelectorAll('.undo-left[data-until]')) u.textContent = `${Math.max(0, Math.ceil((Number(u.dataset.until) - Date.now()) / 1000))} s`;
    }, 15000);
    window.addEventListener('hashchange', () => {
      const m = /^#pair=([0-9a-zA-Z]+)$/.exec(location.hash);
      if (m) { pair(m[1]).then((ok) => ok && start().catch((e) => renderSignIn(e.message))); return; }
      route();
    });
    if ('serviceWorker' in navigator && window.isSecureContext) {
      navigator.serviceWorker.register('sw.js', { scope: './' }).catch(() => {});
    }
    const paired = /^#pair=([0-9a-zA-Z]+)$/.exec(location.hash);
    if (paired && !(await pair(paired[1]))) return;
    state.token = state.token || load(TOKEN_KEY);
    if (!state.token) { setLive('off', 'signed out'); renderSignIn(); return; }
    try {
      await start();
    } catch (e) {
      if (e.status === 401) { signOut('This token is no longer accepted (expired or revoked). Pair again.'); return; }
      setLive('off', 'offline');
      document.body.dataset.view = 'signin';
      main(h('div', { class: 'signin' }, h('h1', { text: 'Cannot reach the server' }), h('p', { class: 'error', text: e.message }),
        h('div', { class: 'row' }, h('button', { type: 'button', class: 'primary', text: 'Try again', onclick: () => location.reload() }))));
      focusHeading();
    }
  }

  boot();
})();
