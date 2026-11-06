/**
 * trajectory.js
 * The console's Trajectory page: what one agent did, step by step, as a
 * waterfall of its turns with its delegation tree under it
 * (docs/companion.md#trajectory).
 *
 * The overview is drawn from each branch's timeline (`GET …/timeline`) and
 * kept current from the event stream by the rules `Timeline::of` reads the
 * log with; it holds no conversation. A step's content is read on demand
 * from the event page, around the one event the step names.
 *
 * Exposes window.BranchyardTrajectory with mount(container, options), and,
 * for the tests that run it under Node, the pure functions it draws from.
 */
(function (root) {
  'use strict';

  const SECOND = 1000;
  const BAD = new Set(['failed', 'interrupted', 'budget_exceeded']);
  const GOOD = new Set(['ready', 'no_changes', 'merged']);
  // Marks that sit at a point rather than span time.
  const POINTS = new Set(['steer', 'steer_rejected', 'delegation', 'judged', 'warning']);
  const PAGE = 200;
  // More tool calls than this in one turn row are drawn as clusters where
  // they are too close to tell apart.
  const MIN_PX = 3;

  // ---------------------------------------------------------------
  // Text

  function fmtDur(ms) {
    if (ms == null || !isFinite(ms)) return '—';
    const n = Math.max(0, Math.round(ms));
    if (n < SECOND) return `${n} ms`;
    if (n < 10 * SECOND) return `${(n / SECOND).toFixed(1)} s`;
    if (n < 60 * SECOND) return `${Math.round(n / SECOND)} s`;
    const s = Math.round(n / SECOND);
    if (s < 3600) return `${Math.floor(s / 60)}m ${String(s % 60).padStart(2, '0')}s`;
    const m = Math.round(s / 60);
    if (m < 1440) return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
    return `${Math.floor(m / 1440)}d ${Math.floor((m % 1440) / 60)}h`;
  }
  const plural = (n, one, many) => `${n} ${n === 1 ? one : many || `${one}s`}`;
  function fmtTokens(n) {
    if (n == null) return '—';
    if (n < 1000) return String(n);
    if (n < 100000) return `${(n / 1000).toFixed(1).replace(/\.0$/, '')}k`;
    return n < 1e6 ? `${Math.round(n / 1000)}k` : `${(n / 1e6).toFixed(1)}M`;
  }
  const cents = (v) => (v == null ? '—' : v > 0 && v < 0.005 ? '<$0.01' : `$${Number(v).toFixed(2)}`);
  const clockOf = (ms) => new Date(ms).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
  const PHASES = ['queued', 'placement', 'image_ready', 'sandbox_ready', 'source_attached', 'harness_started', 'handshake_done', 'first_event'];
  const PHASE_WORDS = {
    queued: 'queued', placement: 'placement', image_ready: 'image ready', sandbox_ready: 'sandbox ready',
    source_attached: 'source attached', harness_started: 'harness started', handshake_done: 'handshake', first_event: 'first event',
  };
  const PATH_WORDS = { cold: 'cold', cached_image: 'cached image', snapshot: 'snapshot', resumed: 'resumed', pool: 'pool', none: 'no sandbox', local: 'on this host' };

  // The phases a startup reached, each with the time it took: its offset
  // less the latest earlier offset, never negative (Startup::durations).
  function phaseDurations(startup) {
    const out = [];
    let before = 0;
    const phases = (startup && startup.phases) || {};
    for (const key of PHASES) {
      if (phases[key] == null) continue;
      out.push({ key, word: PHASE_WORDS[key], offset: phases[key], ms: Math.max(0, phases[key] - before) });
      before = Math.max(before, phases[key]);
    }
    return out;
  }
  const startupTotal = (startup) => phaseDurations(startup).reduce((m, p) => Math.max(m, p.offset), 0);

  // ---------------------------------------------------------------
  // A branch's lane: its timeline, kept current from the stream

  function newTurn(lane, ms, seq) {
    const turn = {
      n: lane.turns.length + 1, seq: seq || 0, start: ms, end: null, outcome: null, cost: null, tokens: null,
      startup: lane.pendingStartup || null, tools: [], marks: [], seen: new Set(),
    };
    lane.pendingStartup = null;
    lane.turns.push(turn);
    return turn;
  }
  function addTool(turn, ms, name, id, seq) {
    const key = `${id}@${ms}`;
    if (turn.seen.has(key)) return;
    turn.seen.add(key);
    turn.tools.push({ at: ms, name, id, seq: seq || 0 });
  }
  function addMark(turn, mark) {
    const key = `${mark.kind}@${mark.at}@${mark.label || ''}`;
    if (turn.seen.has(key)) return;
    turn.seen.add(key);
    turn.marks.push(mark);
  }

  // A lane from a `Timeline` as the server sends it.
  function laneOf(name, timeline) {
    const lane = { name, turns: [], merged: timeline && timeline.merged_at_ms != null ? timeline.merged_at_ms : null, sessionHigh: timeline && timeline.session_tokens != null ? timeline.session_tokens : 0 };
    for (const t of (timeline && timeline.turns) || []) {
      const turn = newTurn(lane, t.started_at_ms, t.seq);
      turn.end = t.ended_at_ms == null ? null : t.ended_at_ms;
      turn.outcome = t.outcome || null;
      turn.cost = t.cost_usd == null ? null : t.cost_usd;
      turn.tokens = t.tokens == null ? null : t.tokens;
      turn.startup = t.startup || null;
      for (const c of t.tools || []) addTool(turn, c.at_ms, c.name, c.call_id, c.seq);
      for (const m of t.marks || []) {
        addMark(turn, { at: m.at_ms, seq: m.seq || 0, kind: m.kind, label: m.label == null ? null : m.label, ok: m.ok == null ? null : m.ok, score: m.score == null ? null : m.score });
      }
    }
    return lane;
  }

  // Keep a lane current from one streamed activity, by the rules
  // `Timeline::of` reads the log with. Every step is idempotent. Returns
  // 'reload' when the activity rewrites the turns, so the caller reads the
  // timeline again; otherwise whether anything changed.
  function fold(lane, a, ms) {
    if (!a || typeof a !== 'object') {
      if (a === 'resumed') return markLast(lane, { at: ms, seq: 0, kind: 'resumed', label: null, ok: null, score: null });
      return false;
    }
    if (a.rewound || a.recovered) return 'reload';
    const last = lane.turns[lane.turns.length - 1];
    const open = !!last && last.end == null;
    const ev = a.harness || {};
    if (a.prompt != null) {
      if (lane.turns.some((t) => t.start === ms)) return false;
      newTurn(lane, ms, 0);
      return true;
    }
    if (a.steered) {
      let turn = last;
      if (!open) {
        if (lane.turns.some((t) => t.start === ms)) return false;
        turn = newTurn(lane, ms, 0);
      }
      addMark(turn, { at: ms, seq: 0, kind: 'steer', label: null, ok: null, score: null });
      return true;
    }
    if (ev.type === 'tool_started') {
      addTool(open ? last : newTurn(lane, ms, 0), ms, ev.name, ev.call_id, 0);
      return true;
    }
    if (ev.type === 'permission_requested') return markLast(lane, { at: ms, seq: 0, kind: 'permission', label: ev.request ? ev.request.tool : null, ok: null, score: null });
    if (ev.type === 'steer_rejected') return markLast(lane, { at: ms, seq: 0, kind: 'steer_rejected', label: null, ok: null, score: null });
    if (ev.type === 'warning' || a.warning != null) return markLast(lane, { at: ms, seq: 0, kind: 'warning', label: null, ok: null, score: null });
    if (ev.type === 'session_started') { lane.sessionHigh = 0; return false; }
    if (ev.type === 'usage_observed' && ev.usage && last) {
      const u = ev.usage;
      if (u.input_tokens == null && u.output_tokens == null) return false;
      const key = `usage@${ms}@${u.input_tokens}@${u.output_tokens}@${u.cumulative}`;
      if (last.seen.has(key)) return false;
      last.seen.add(key);
      const n = (u.input_tokens || 0) + (u.output_tokens || 0);
      let grown = n;
      if (u.cumulative) { grown = Math.max(0, n - lane.sessionHigh); lane.sessionHigh = Math.max(lane.sessionHigh, n); }
      last.tokens = (last.tokens || 0) + grown;
      return true;
    }
    if (a.decision) return markLast(lane, { at: ms, seq: 0, kind: 'decision', label: a.decision.tool, ok: !!a.decision.allowed, score: null });
    if (a.stalled) return markLast(lane, { at: ms, seq: 0, kind: 'stall', label: null, ok: null, score: null });
    if (a.delegation) return markLast(lane, { at: ms, seq: 0, kind: 'delegation', label: a.delegation.branch || null, ok: !a.delegation.refused, score: null });
    if (a.fleet && a.fleet.type === 'judged') return markLast(lane, { at: ms, seq: 0, kind: 'judged', label: null, ok: null, score: a.fleet.score });
    if (a.startup) {
      if (last) last.startup = a.startup; else lane.pendingStartup = a.startup;
      return true;
    }
    if (a.turn_cost && last) { last.cost = a.turn_cost.cost_usd; return true; }
    if (a.status) {
      if (a.status.state === 'merged') lane.merged = ms;
      if (open && !['running', 'waiting'].includes(a.status.state)) { last.end = ms; last.outcome = a.status; }
      return true;
    }
    return false;
  }
  function markLast(lane, mark) {
    const last = lane.turns[lane.turns.length - 1];
    if (!last) return false;
    const before = last.marks.length;
    addMark(last, mark);
    return last.marks.length !== before;
  }

  // ---------------------------------------------------------------
  // A turn as steps

  // A turn's time tiled into what the agent was doing: the model (between
  // calls), a tool call from its start to the next thing recorded (the next
  // call, a permission request or a stall), a wait for a person's answer,
  // a stall. A step the person can select is each of these, each point
  // mark, the prompt, the startup and the end.
  function stepsOf(lane, turn, now) {
    let lastAt = turn.start;
    for (const t of turn.tools) lastAt = Math.max(lastAt, t.at);
    for (const m of turn.marks) lastAt = Math.max(lastAt, m.at);
    const end = turn.end != null ? turn.end : Math.max(now, lastAt);
    const items = [];
    turn.tools.forEach((tool, i) => items.push({ at: tool.at, order: i, type: 'tool', tool }));
    turn.marks.forEach((mark, i) => {
      if (['permission', 'decision', 'stall', 'resumed'].includes(mark.kind)) items.push({ at: mark.at, order: 1e6 + i, type: mark.kind, mark });
    });
    items.sort((x, y) => x.at - y.at || x.order - y.order);
    const segs = [];
    let state = { kind: 'model' };
    let before = state;
    let from = turn.start;
    const cut = (to, next, final) => {
      // A wait or a stall that something else interrupted is not still open.
      if (!final && state.open) state.open = false;
      const at = Math.max(from, Math.min(to, end));
      if (at > from || state.kind !== 'model') segs.push({ ...state, from, to: at });
      from = at;
      state = next;
    };
    for (const it of items) {
      if (it.type === 'tool') cut(it.at, { kind: 'tool', tool: it.tool });
      else if (it.type === 'permission') { before = state; cut(it.at, { kind: 'wait', mark: it.mark, open: true }); }
      else if (it.type === 'decision') {
        if (state.kind === 'wait') {
          state.decision = it.mark;
          state.open = false;
          cut(it.at, before.kind === 'tool' ? { kind: 'tool', tool: before.tool, continued: true } : before);
        }
      } else if (it.type === 'stall') { before = state; cut(it.at, { kind: 'stall', mark: it.mark, open: true }); }
      else if (it.type === 'resumed') {
        if (state.kind === 'stall') {
          state.open = false;
          cut(it.at, before.kind === 'tool' ? { kind: 'tool', tool: before.tool, continued: true } : before);
        }
      }
    }
    cut(end, state, true);
    const steps = [];
    steps.push({ type: 'prompt', at: turn.start, to: turn.start, seq: turn.seq, turn });
    if (turn.startup) steps.push({ type: 'startup', at: turn.start, to: turn.start, startup: turn.startup, turn, before: true });
    for (const s of segs) {
      if (s.kind === 'tool' && s.continued) continue;
      steps.push({
        type: s.kind, at: s.from, to: s.to, turn, seg: s, open: !!s.open,
        seq: s.kind === 'tool' ? s.tool.seq : s.kind === 'model' ? 0 : s.mark.seq,
        name: s.kind === 'tool' ? s.tool.name : s.kind === 'wait' ? s.mark.label : null,
        callId: s.kind === 'tool' ? s.tool.id : null,
      });
    }
    for (const m of turn.marks) {
      if (POINTS.has(m.kind)) steps.push({ type: m.kind, at: m.at, to: m.at, seq: m.seq, mark: m, turn, name: m.label });
    }
    if (turn.end != null) steps.push({ type: 'end', at: turn.end, to: turn.end, turn, outcome: turn.outcome, bad: !!(turn.outcome && BAD.has(turn.outcome.state)) });
    const rank = { prompt: 0, startup: 0.5, model: 1, tool: 2, wait: 2, stall: 2, end: 9 };
    steps.sort((x, y) => x.at - y.at || (rank[x.type] == null ? 5 : rank[x.type]) - (rank[y.type] == null ? 5 : rank[y.type]));
    steps.forEach((s, i) => { s.index = i; s.lane = lane.name; s.id = `${lane.name}|${turn.n}|${s.type}|${s.at}|${s.callId || s.seq || ''}`; });
    return { steps, segs, end };
  }

  // What a turn spent its time on.
  function split(lane, turn, now) {
    const { segs, end } = stepsOf(lane, turn, now);
    const out = { tools: 0, model: 0, wait: 0, stall: 0, startup: turn.startup ? startupTotal(turn.startup) : 0, span: Math.max(0, end - turn.start) };
    for (const s of segs) out[s.kind === 'tool' ? 'tools' : s.kind] += s.to - s.from;
    return out;
  }

  // The whole branch: totals over its turns.
  function summarize(lane, now) {
    const sum = { turns: lane.turns.length, span: 0, tools: 0, model: 0, wait: 0, stall: 0, startup: 0, calls: 0, tokens: null, cost: null, first: null, last: null, startupLatest: null };
    for (const t of lane.turns) {
      const s = split(lane, t, now);
      for (const k of ['span', 'tools', 'model', 'wait', 'stall', 'startup']) sum[k] += s[k];
      sum.calls += t.tools.length;
      if (t.tokens != null) sum.tokens = (sum.tokens || 0) + t.tokens;
      if (t.cost != null) sum.cost = (sum.cost || 0) + t.cost;
      if (t.startup) sum.startupLatest = t.startup;
      sum.first = sum.first == null ? t.start : Math.min(sum.first, t.start);
      sum.last = Math.max(sum.last || 0, t.end != null ? t.end : now);
    }
    return sum;
  }

  // ---------------------------------------------------------------
  // The time axis: real time, with long idle gaps between the work folded

  function niceStep(ms) {
    if (!(ms > 0)) return 1;
    const mag = 10 ** Math.floor(Math.log10(ms));
    for (const m of [1, 2, 5, 10]) if (m * mag >= ms) return m * mag;
    return 10 * mag;
  }

  // `intervals` are [from, to] real times of work. With `compress`, a gap
  // between them longer than a quarter of the work (and 15 s) is drawn as a
  // short break, so a branch that waited hours for its person still reads.
  function buildScale(intervals, now, compress) {
    const iv = intervals.filter(([a, b]) => isFinite(a) && isFinite(b)).map(([a, b]) => [a, Math.max(a, b)]).sort((x, y) => x[0] - y[0]);
    const merged = [];
    for (const [a, b] of iv) {
      const m = merged[merged.length - 1];
      if (m && a <= m[1]) m[1] = Math.max(m[1], b); else merged.push([a, b]);
    }
    if (!merged.length) merged.push([now - SECOND, now]);
    if (merged[0][1] <= merged[0][0]) merged[0][1] = merged[0][0] + SECOND;
    const work = merged.reduce((n, [a, b]) => n + (b - a), 0);
    const gapMin = Math.max(15 * SECOND, work * 0.25);
    const breakLen = Math.max(1500, work * 0.04);
    const pieces = [];
    let v = 0;
    for (let i = 0; i < merged.length; i++) {
      const [a, b] = merged[i];
      const next = merged[i + 1];
      const gap = next ? next[0] - b : 0;
      const folded = compress && next && gap > gapMin;
      const end = next && !folded ? next[0] : b;
      pieces.push({ r0: a, r1: end, v0: v, len: end - a, brk: false });
      v += end - a;
      if (folded) {
        pieces.push({ r0: b, r1: next[0], v0: v, len: breakLen, brk: true });
        v += breakLen;
      }
    }
    const first = merged[0][0];
    const toV = (ms) => {
      if (ms <= pieces[0].r0) return 0;
      for (const p of pieces) {
        if (ms <= p.r1) return p.brk ? p.v0 + ((ms - p.r0) / (p.r1 - p.r0)) * p.len : p.v0 + (ms - p.r0);
      }
      const last = pieces[pieces.length - 1];
      return last.v0 + last.len + (ms - last.r1);
    };
    // Active elapsed time at a virtual position: the breaks count for none.
    const elapsed = (vv) => {
      let brk = 0;
      for (const p of pieces) if (p.brk && p.v0 + p.len <= vv) brk += p.len;
      return vv - brk;
    };
    return { pieces, total: Math.max(v, 1), toV, elapsed, first, work };
  }

  // The axis ticks for a track `px` wide: every nice step at least ~84 px
  // apart, none inside a break.
  function ticksOf(scale, px) {
    const perMs = px / scale.total;
    const step = niceStep(84 / perMs);
    const out = [];
    for (let v = 0; v <= scale.total + 1e-6; v += step) {
      if (scale.pieces.some((p) => p.brk && v > p.v0 && v < p.v0 + p.len)) continue;
      out.push({ v, label: fmtDur(scale.elapsed(v)) });
      if (out.length > 400) break;
    }
    return out;
  }

  // ---------------------------------------------------------------
  // A fan's attempts: `by fan` starts top-level branches named
  // `<name>-<harness>` (or `-<harness>-<n>`) with one prompt
  // (Yard::fan_branches); a parent's children are siblings already.

  function fanGroup(branches, name) {
    const me = branches.get(name);
    if (!me) return [];
    if (me.parent) {
      const parent = branches.get(me.parent);
      return parent && parent.children && parent.children.length > 1 ? parent.children.slice() : [];
    }
    const top = Array.from(branches.values()).filter((b) => !b.parent && (b.depth || 0) === 0);
    const stems = new Set();
    for (const id of [me.harness, me.profile].filter(Boolean)) {
      for (const pattern of [new RegExp(`^(.+)-${escapeRe(id)}$`), new RegExp(`^(.+)-${escapeRe(id)}-\\d+$`)]) {
        const m = pattern.exec(me.name);
        if (m) stems.add(m[1]);
      }
    }
    for (const stem of stems) {
      const group = top.filter((b) => b.prompt === me.prompt && [b.harness, b.profile].filter(Boolean).some((id) => b.name === `${stem}-${id}` || new RegExp(`^${escapeRe(stem)}-${escapeRe(id)}-\\d+$`).test(b.name)));
      if (group.length > 1) return group.sort((x, y) => x.created_at - y.created_at).map((b) => b.name);
    }
    return [];
  }
  function escapeRe(s) { return String(s).replace(/[.*+?^${}()|[\]\\]/g, '\\$&'); }

  // ---------------------------------------------------------------
  // The page

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

  const GLYPH = { prompt: '▸', steer: '↳', steer_rejected: '✕', delegation: '⑂', judged: '★', warning: '!', end: '■' };
  const KIND_WORDS = {
    prompt: 'Prompt', startup: 'Startup', model: 'Model', tool: 'Tool call', wait: 'Waiting for a person', stall: 'Stalled',
    steer: 'Steered by a person', steer_rejected: 'Steer refused', delegation: 'Delegated', judged: 'Judged', warning: 'Warning', end: 'Turn ended',
  };
  const outcomeWord = (o) => (o ? ({ ready: 'ready', no_changes: 'no changes', merged: 'merged', failed: 'failed', interrupted: 'interrupted', budget_exceeded: 'over budget', blocked: 'blocked' }[o.state] || String(o.state).replace(/_/g, ' ')) : 'running');
  const outcomeGlyph = (o) => (!o ? '…' : BAD.has(o.state) ? '✕' : GOOD.has(o.state) ? '✓' : '•');

  function mount(container, opts) {
    const view = {
      lanes: new Map(),      // name -> { lane, loading, error, held }
      expanded: new Set(),
      zoom: 1,
      compress: true,
      selected: null,        // a step id
      focusId: null,
      detail: null,          // { id, state, nodes }
      compare: false,
      destroyed: false,
      timer: null,
      draw: null,
    };
    const branches = () => opts.branches();
    const now = () => (opts.now ? opts.now() : Date.now());

    // ----- lanes -----
    function ensureLane(name) {
      let entry = view.lanes.get(name);
      if (entry) return entry;
      entry = { name, lane: null, loading: false, error: null, held: [], tries: 0 };
      view.lanes.set(name, entry);
      readLane(entry);
      return entry;
    }
    function readLane(entry) {
      entry.loading = true;
      entry.held = [];
      opts.api(`${opts.path(entry.name)}/timeline`)
        .then((timeline) => {
          if (view.destroyed) return;
          const lane = laneOf(entry.name, timeline);
          // Events that arrived while the timeline was read may or may not
          // be in it: read again rather than guess, so counts stay exact.
          if (entry.held.length && entry.tries < 3) { entry.tries += 1; entry.lane = lane; readLane(entry); return; }
          entry.tries = 0;
          entry.lane = lane;
          entry.error = null;
          for (const [a, ms] of entry.held) fold(lane, a, ms);
          entry.held = [];
          entry.loading = false;
          schedule();
        })
        .catch((e) => {
          if (view.destroyed) return;
          entry.error = e && e.message ? e.message : 'could not be read';
          entry.loading = false;
          schedule();
        });
    }
    function childrenOf(name) {
      const info = branches().get(name);
      return info && info.children ? info.children.filter((c) => branches().has(c)) : [];
    }
    // The branches the page shows: the root, its children, and the
    // children of every expanded one.
    function shown() {
      const out = [];
      const walk = (name, depth) => {
        out.push({ name, depth });
        if (depth === 0 || view.expanded.has(name)) for (const c of childrenOf(name)) walk(c, depth + 1);
      };
      walk(opts.branch, 0);
      return out;
    }

    // ----- live -----
    function activity(branch, a, ms) {
      const entry = view.lanes.get(branch);
      if (!entry) return;
      if (entry.loading) { entry.held.push([a, ms]); return; }
      if (!entry.lane) return;
      const result = fold(entry.lane, a, ms);
      if (result === 'reload') { readLane(entry); return; }
      if (result) schedule();
    }
    function branchesChanged() {
      if (view.destroyed) return;
      for (const s of shown()) ensureLane(s.name);
      schedule();
    }

    let queued = false;
    function schedule() {
      if (queued || view.destroyed) return;
      queued = true;
      requestAnimationFrame(() => { queued = false; if (!view.destroyed) render(); });
    }

    // ----- layout -----
    const ui = {};
    function build() {
      ui.summary = h('section', { class: 'tj-summary', 'aria-label': 'Summary' });
      ui.toolbar = h('div', { class: 'tj-toolbar', role: 'toolbar', 'aria-label': 'Waterfall controls' });
      ui.scroll = h('div', { class: 'tj-scroll', role: 'group', tabindex: '-1', 'aria-label': 'Waterfall of turns, tool calls and waits. Arrow keys move between steps.' });
      ui.detail = h('aside', { class: 'tj-detail', 'aria-label': 'Step detail', 'aria-live': 'polite' });
      ui.compare = h('section', { class: 'tj-compare', 'aria-label': 'Comparison', hidden: true });
      ui.list = h('details', { class: 'tj-listing' }, h('summary', { text: 'Steps as a list' }), h('div', { class: 'tj-listing-body' }));
      ui.hint = h('p', { class: 'tj-hint hint' });
      container.replaceChildren(
        ui.summary,
        h('div', { class: 'tj-main' },
          h('div', { class: 'tj-chart' }, ui.toolbar, ui.scroll, ui.hint),
          ui.detail),
        ui.compare, ui.list);
      buildToolbar();
      ui.scroll.addEventListener('keydown', onKey);
      ui.list.addEventListener('toggle', () => { if (ui.list.open) renderList(); });
      ui.resize = typeof ResizeObserver === 'function' ? new ResizeObserver(() => schedule()) : null;
      if (ui.resize) ui.resize.observe(ui.scroll);
    }

    function render() {
      const focusedId = ui.scroll.contains(document.activeElement) && document.activeElement.dataset ? document.activeElement.dataset.step : null;
      const rootEntry = view.lanes.get(opts.branch);
      renderSummary(rootEntry);
      renderToolbar();
      renderChart(rootEntry);
      renderCompare();
      renderDetailEmpty();
      if (ui.list.open) renderList();
      const wantFocus = focusedId || null;
      if (wantFocus) {
        const again = ui.scroll.querySelector(`[data-step="${CSS.escape(wantFocus)}"]`);
        if (again) again.focus({ preventScroll: true });
      }
      clearTimeout(view.timer);
      const running = Array.from(view.lanes.values()).some((e) => e.lane && e.lane.turns.some((t) => t.end == null));
      if (running) view.timer = setTimeout(schedule, 1000);
      container.setAttribute('data-ready', rootEntry && rootEntry.lane ? 'true' : 'false');
    }

    // ----- summary -----
    function renderSummary(entry) {
      const info = branches().get(opts.branch);
      if (!entry || !entry.lane) {
        ui.summary.replaceChildren(h('p', { class: entry && entry.error ? 'error' : 'muted', role: entry && entry.error ? 'alert' : 'status', text: entry && entry.error ? `The timeline could not be read: ${entry.error}` : 'Reading the timeline…' }));
        return;
      }
      const lane = entry.lane;
      const sum = summarize(lane, now());
      const status = info ? opts.status(info) : { key: 'unknown', text: 'unknown' };
      const lastTurn = lane.turns[lane.turns.length - 1];
      const outcome = info ? info.status : lastTurn ? (lastTurn.end == null ? { state: 'running' } : lastTurn.outcome) : null;
      const st = sum.startupLatest;
      const startup = st ? `${st.provider} · ${PATH_WORDS[st.path] || st.path} · ${fmtDur(startupTotal(st))} to the first event` : '—';
      const parts = [
        ['tools', 'Tools', sum.tools], ['model', 'Model', sum.model], ['wait', 'Waiting for a person', sum.wait + sum.stall], ['startup', 'Startup', sum.startup],
      ];
      const whole = parts.reduce((n, p) => n + p[2], 0) || 1;
      const bar = h('div', { class: 'tj-split', role: 'img', 'aria-label': `Time: ${parts.map(([, w, ms]) => `${w} ${Math.round((ms / whole) * 100)} percent, ${fmtDur(ms)}`).join('; ')}` });
      for (const [k, w, ms] of parts) {
        if (ms <= 0) continue;
        const seg = h('i', { class: `tj-split-${k}`, title: `${w}: ${fmtDur(ms)}` });
        seg.style.flexGrow = String(Math.max(0.5, (ms / whole) * 100));
        bar.append(seg);
      }
      const stat = (label, value, extra) => h('div', { class: 'tj-stat' }, h('dt', { text: label }), h('dd', { 'data-stat': label.toLowerCase().replace(/[^a-z]+/g, '-'), title: extra || null, text: value }));
      ui.summary.replaceChildren(
        h('dl', { class: 'tj-stats' },
          stat('Harness', info ? info.harness : '—'),
          stat('Model', info && info.model ? info.model : '—'),
          stat('Effort', info && info.effort ? info.effort : '—', info && info.effort ? null : 'the branch record does not say'),
          stat('Turns', String(sum.turns)),
          stat('Duration', fmtDur(sum.span), 'turns only; idle time between them is not counted'),
          stat('Tool calls', String(sum.calls)),
          stat('Tokens', fmtTokens(sum.tokens), sum.tokens == null ? 'the harness reported none' : `${sum.tokens} input and output tokens`),
          stat('Cost', cents(info && info.cost_usd != null ? info.cost_usd : sum.cost), 'what the harness reported'),
          stat('Outcome', `${outcomeGlyph(outcome && ['running', 'waiting', 'waiting_on_children'].includes(outcome.state) ? null : outcome)} ${status.text}`),
          stat('Startup', startup)),
        h('div', { class: 'tj-timeuse' },
          h('p', { class: 'tj-timeuse-title', text: 'Where the time went' }),
          bar,
          h('ul', { class: 'tj-legend' }, parts.map(([k, w, ms]) => h('li', {}, h('i', { class: `tj-key tj-split-${k}`, 'aria-hidden': 'true' }), `${w} `, h('b', { text: fmtDur(ms) }), ` · ${Math.round((ms / whole) * 100)}%`)))));
    }

    // ----- toolbar -----
    function buildToolbar() {
      const button = (label, onclick, props) => h('button', { type: 'button', class: 'small', onclick, ...props }, label);
      ui.zoomval = h('output', { class: 'tj-zoomval', 'aria-label': 'Zoom level', text: '100%' });
      ui.compareBtn = button('Compare', () => { view.compare = !view.compare; for (const n of compareSet().names) ensureLane(n); schedule(); }, { hidden: true });
      ui.toolbar.replaceChildren(
        h('span', { class: 'tj-zoom', role: 'group', 'aria-label': 'Zoom' },
          button('−', () => zoom(1 / 1.6), { 'aria-label': 'Zoom out', title: 'Zoom out (−)' }),
          ui.zoomval,
          button('+', () => zoom(1.6), { 'aria-label': 'Zoom in', title: 'Zoom in (+)' }),
          button('Fit', () => { view.zoom = 1; schedule(); }, { title: 'Fit the width (0)' })),
        h('label', { class: 'tj-check' },
          h('input', { type: 'checkbox', checked: true, onchange: (ev) => { view.compress = ev.target.checked; schedule(); } }),
          ' Fold long idle gaps'),
        ui.compareBtn);
      ui.hint.textContent = 'Arrow keys move between steps, Up and Down between turns and lanes, Enter or Space opens a step, + and − zoom, 0 fits, Escape closes the detail.';
    }
    // The branches to set side by side: a fan's attempts, siblings under
    // one parent, else this branch's own children.
    function compareSet() {
      const group = fanGroup(branches(), opts.branch);
      if (group.length > 1) return { names: group, word: 'attempts' };
      const kids = childrenOf(opts.branch);
      return kids.length > 1 ? { names: kids, word: 'children' } : { names: [], word: '' };
    }
    function renderToolbar() {
      ui.zoomval.textContent = `${Math.round(view.zoom * 100)}%`;
      const set = compareSet();
      ui.compareBtn.hidden = set.names.length < 2;
      ui.compareBtn.textContent = view.compare ? 'Hide comparison' : `Compare ${set.names.length} ${set.word}`;
      ui.compareBtn.setAttribute('aria-pressed', String(view.compare));
    }
    function zoom(factor) {
      view.zoom = Math.max(1, Math.min(200, view.zoom * factor));
      schedule();
      const sel = view.selected && ui.scroll.querySelector(`[data-step="${CSS.escape(view.selected)}"]`);
      requestAnimationFrame(() => {
        const again = view.selected && ui.scroll.querySelector(`[data-step="${CSS.escape(view.selected)}"]`);
        if (again && again.scrollIntoView) again.scrollIntoView({ inline: 'center', block: 'nearest' });
      });
      return sel;
    }

    // Fit the width to one turn, with the chart scrolled to its start.
    function zoomToTurn(lane, turn) {
      const last = view.last;
      if (!last) return;
      const lead = turn.startup ? (turn.startup.phases || {}).handshake_done || startupTotal(turn.startup) : 0;
      const end = stepsOf(lane, turn, now()).end;
      const v0 = last.scale.toV(turn.start - lead);
      const v1 = last.scale.toV(end);
      const span = Math.max(v1 - v0, last.scale.total / 200);
      view.zoom = Math.max(1, Math.min(200, (0.92 * last.scale.total) / span));
      schedule();
      requestAnimationFrame(() => requestAnimationFrame(() => {
        const l = view.last;
        if (l) ui.scroll.scrollLeft = Math.max(0, v0 * l.perV - 8);
      }));
    }

    // ----- chart -----
    function renderChart(entry) {
      const LABEL_PX = ui.scroll.clientWidth < 520 ? 124 : 168;
      if (!entry || !entry.lane) { ui.scroll.replaceChildren(h('p', { class: 'muted pad', text: entry && entry.error ? '' : 'Reading the timeline…' })); return; }
      const t = now();
      const rows = [];
      const intervals = [];
      for (const s of shown()) {
        const e = view.lanes.get(s.name);
        if (!e || !e.lane) continue;
        for (const turn of e.lane.turns) {
          const lead = turn.startup ? startupTotal(turn.startup) : 0;
          const handshake = turn.startup ? (turn.startup.phases || {}).handshake_done : null;
          intervals.push([turn.start - (handshake != null ? handshake : lead), stepsOf(e.lane, turn, t).end]);
        }
        if (e.lane.merged != null) intervals.push([e.lane.merged, e.lane.merged]);
      }
      const scale = buildScale(intervals, t, view.compress);
      const avail = Math.max(240, ui.scroll.clientWidth - LABEL_PX - 24);
      const px = Math.round(avail * view.zoom);
      const perV = px / scale.total;
      const x = (ms) => scale.toV(ms) * perV;
      view.last = { scale, perV, px, avail };
      ui.scroll.style.setProperty('--tj-label', `${LABEL_PX}px`);
      ui.scroll.style.setProperty('--tj-w', `${px}px`);

      const axisTrack = h('div', { class: 'tj-track' });
      for (const tick of ticksOf(scale, px)) {
        const el = h('span', { class: 'tj-tick', text: tick.label });
        el.style.left = `${tick.v * perV}px`;
        axisTrack.append(el);
      }
      breaksIn(axisTrack, scale, perV, true);
      rows.push(h('div', { class: 'tj-row tj-axis', 'aria-hidden': 'true' }, h('div', { class: 'tj-label', text: 'Elapsed' }), axisTrack));
      rows.push(...turnRows({ name: opts.branch, depth: 0 }, entry, scale, x, perV, t));
      ui.scroll.replaceChildren(...rows);
      // The one tab stop: the selected step, else the first.
      const buttons = Array.from(ui.scroll.querySelectorAll('[data-step]'));
      const stop = buttons.find((b) => b.dataset.step === (view.focusId || view.selected)) || buttons[0];
      for (const b of buttons) b.tabIndex = b === stop ? 0 : -1;
    }

    function breaksIn(track, scale, perV, label) {
      for (const p of scale.pieces) {
        if (!p.brk) continue;
        const el = h('i', { class: 'tj-break', title: `${fmtDur(p.r1 - p.r0)} idle, folded`, 'aria-hidden': 'true' });
        el.style.left = `${p.v0 * perV}px`;
        el.style.width = `${p.len * perV}px`;
        if (label) el.textContent = fmtDur(p.r1 - p.r0);
        track.append(el);
      }
    }

    // A branch that is the root: one row per turn.
    function turnRows(s, e, scale, x, perV, t) {
      const rows = [];
      e.lane.turns.forEach((turn) => {
        rows.push(turnRow(e.lane, turn, scale, x, perV, t, s.depth));
        // A child nested under the turn that spawned it.
        for (const child of spawnedIn(e.lane, turn)) {
          if (!view.lanes.has(child)) continue;
          rows.push(...childRows({ name: child, depth: s.depth + 1 }, view.lanes.get(child), scale, x, perV, t));
        }
      });
      // Children whose spawn the log does not show hang under the last turn.
      for (const child of orphans(e.lane, s.name)) {
        if (view.lanes.has(child)) rows.push(...childRows({ name: child, depth: s.depth + 1 }, view.lanes.get(child), scale, x, perV, t));
      }
      if (!e.lane.turns.length) rows.push(h('div', { class: 'tj-row' }, h('div', { class: 'tj-label', text: 'No turns yet' }), h('div', { class: 'tj-track' })));
      return rows;
    }
    function spawnedIn(lane, turn) {
      return turn.marks.filter((m) => m.kind === 'delegation' && m.ok !== false && m.label && branches().has(m.label)).map((m) => m.label);
    }
    function orphans(lane, name) {
      const named = new Set();
      for (const turn of lane.turns) for (const c of spawnedIn(lane, turn)) named.add(c);
      return childrenOf(name).filter((c) => !named.has(c));
    }

    function childRows(s, e, scale, x, perV, t) {
      const info = branches().get(s.name);
      const open = view.expanded.has(s.name);
      const kids = childrenOf(s.name);
      const labelId = `tj-lane-${s.name.replace(/[^A-Za-z0-9_-]/g, '_')}`;
      const status = info ? opts.status(info) : { text: '' };
      const lane = e && e.lane;
      const summary = lane ? summarize(lane, t) : null;
      const judged = lane ? judgeOf(lane) : null;
      const toggle = h('button', {
        type: 'button', class: 'tj-toggle', 'aria-expanded': String(open), 'aria-controls': labelId, 'data-lane': s.name,
        'aria-label': `${open ? 'Collapse' : 'Expand'} ${s.name}`,
        onclick: () => { view.expanded.has(s.name) ? view.expanded.delete(s.name) : view.expanded.add(s.name); for (const x2 of shown()) ensureLane(x2.name); schedule(); },
      }, open ? '▾' : '▸');
      const label = h('div', { class: 'tj-label tj-lane-label', id: labelId, 'data-depth': String(s.depth) },
        toggle,
        h('span', { class: 'tj-lane-id' },
          h('a', { href: opts.href.trajectory(s.name), text: s.name }),
          h('span', { class: 'tj-lane-meta', text: [info && info.harness, info && info.model, status.text].filter(Boolean).join(' · ') }),
          judged != null ? h('span', { class: 'tj-score', title: 'The judge\'s score, 0 to 100', text: `score ${Math.round(judged)}` }) : null));
      label.style.paddingLeft = `${Math.min(s.depth, 4) * 12 + 4}px`;
      const rows = [];
      const track = h('div', { class: 'tj-track' });
      if (!lane) {
        track.append(h('span', { class: 'tj-note', text: e && e.error ? `could not be read: ${e.error}` : 'reading…' }));
        rows.push(h('div', { class: 'tj-row tj-lane tj-child', 'data-lane': s.name }, label, track));
        return rows;
      }
      if (!open) {
        // Collapsed: each of its turns as one bar.
        for (const turn of lane.turns) {
          const end = turn.end != null ? turn.end : t;
          const stp = stepsOf(lane, turn, t).steps.find((q) => q.type === 'prompt');
          const bad = turn.outcome && BAD.has(turn.outcome.state);
          const b = h('button', {
            type: 'button', class: `tj-seg tj-sum${bad ? ' tj-bad' : ''}${turn.end == null ? ' tj-live' : ''}`, 'data-step': `${s.name}|sum|${turn.n}`, 'data-kind': 'child-turn',
            'aria-label': `${s.name}, turn ${turn.n}, ${fmtDur(end - turn.start)}, ${outcomeWord(turn.end == null ? null : turn.outcome)}, ${plural(turn.tools.length, 'tool call')}`,
            onclick: () => select({ id: `${s.name}|sum|${turn.n}`, type: 'child-turn', lane: s.name, at: turn.start, to: end, turn, seq: turn.seq, step: stp }),
          }, h('span', { class: 'tj-seg-text' }, `${bad ? '✕ ' : turn.end != null ? '✓ ' : ''}turn ${turn.n}`));
          place(b, x(turn.start), x(end));
          track.append(b);
        }
        breaksIn(track, scale, perV);
        rows.push(h('div', { class: 'tj-row tj-lane tj-child', 'data-lane': s.name }, label, track));
        return rows;
      }
      rows.push(h('div', { class: 'tj-row tj-lane tj-child tj-child-head', 'data-lane': s.name }, label, h('div', { class: 'tj-track' }, h('span', { class: 'tj-note', text: `${plural(summary.turns, 'turn')} · ${fmtDur(summary.span)} · ${plural(summary.calls, 'tool call')}${kids.length ? ` · delegated to ${kids.length}` : ''}` }))));
      rows.push(...turnRows(s, e, scale, x, perV, t));
      return rows;
    }
    function judgeOf(lane) {
      let score = null;
      for (const turn of lane.turns) for (const m of turn.marks) if (m.kind === 'judged' && m.score != null) score = m.score;
      return score;
    }

    function place(el, x0, x1) {
      el.style.left = `${x0}px`;
      el.style.width = `${Math.max(MIN_PX, x1 - x0)}px`;
    }

    function turnRow(lane, turn, scale, x, perV, t, depth) {
      const { steps, segs, end } = stepsOf(lane, turn, t);
      const span = end - turn.start;
      const bad = turn.outcome && BAD.has(turn.outcome.state);
      const label = h('div', { class: 'tj-label tj-turn-label', 'data-depth': String(depth) },
        h('span', { class: 'tj-turn-name' },
          h('button', { type: 'button', class: 'tj-turn-zoom', 'aria-label': `Zoom to turn ${turn.n}${lane.name === opts.branch ? '' : ` of ${lane.name}`}`, title: 'Zoom to this turn', onclick: () => zoomToTurn(lane, turn) }, `Turn ${turn.n}`),
          bad ? h('span', { class: 'tj-badge tj-badge-bad', text: ` ✕ ${outcomeWord(turn.outcome)}` }) : turn.end == null ? h('span', { class: 'tj-badge tj-badge-live', text: ' running' }) : null),
        h('span', { class: 'tj-turn-meta', text: [fmtDur(span), turn.cost != null ? cents(turn.cost) : null, turn.tokens != null ? `${fmtTokens(turn.tokens)} tok` : null].filter(Boolean).join(' · ') }));
      label.style.paddingLeft = `${Math.min(depth, 4) * 12 + 8}px`;
      const track = h('div', { class: 'tj-track' });
      breaksIn(track, scale, perV);
      const total = steps.length;
      const stepButton = (step, cls, text, x0, x1, extra) => {
        const b = h('button', {
          type: 'button', class: `tj-seg ${cls}`, 'data-step': step.id, 'data-kind': step.type, 'data-turn': String(turn.n),
          'aria-label': stepLabel(lane, turn, step, total),
          'aria-pressed': String(view.selected === step.id),
          title: stepLabel(lane, turn, step, total),
          onclick: () => select(step),
          ...extra,
        }, text);
        place(b, x0, x1);
        return b;
      };
      // startup: a band that ends where the harness's first event came,
      // with the handshake laid at the prompt (the prompt is sent just
      // after it), each phase a slice of it
      if (turn.startup) {
        const lead = startupTotal(turn.startup);
        const handshake = (turn.startup.phases || {}).handshake_done;
        const anchor = turn.start - (handshake != null ? handshake : lead);
        const step = steps.find((q) => q.type === 'startup');
        const x0 = x(anchor);
        const b = stepButton(step, 'tj-startup', h('span', { class: 'tj-seg-text', text: `startup ${fmtDur(lead)}` }), x0, x(anchor + lead));
        let at = anchor;
        for (const p of phaseDurations(turn.startup)) {
          const piece = h('i', { class: 'tj-phase', title: `${p.word}: ${fmtDur(p.ms)}`, 'aria-hidden': 'true' });
          piece.style.left = `${x(at) - x0}px`;
          piece.style.width = `${Math.max(1, x(at + p.ms) - x(at))}px`;
          b.append(piece);
          at += p.ms;
        }
        track.append(b);
      }
      // what a wait or a stall interrupted carries on after it, drawn
      // without a step of its own
      for (const seg of segs) {
        if (seg.kind === 'tool' && seg.continued) {
          const cont = h('i', { class: 'tj-seg tj-tool tj-cont', 'aria-hidden': 'true' });
          place(cont, x(seg.from), x(seg.to));
          track.append(cont);
        }
      }
      // segments
      const kinds = { model: 'tj-model', tool: 'tj-tool', wait: 'tj-wait', stall: 'tj-stall' };
      let cluster = null;
      const flush = () => {
        if (!cluster) return;
        const first = cluster.items[0];
        const lastItem = cluster.items[cluster.items.length - 1];
        if (cluster.items.length === 1) {
          const q = first;
          track.append(stepButton(q.step, 'tj-tool', h('span', { class: 'tj-seg-text', text: q.step.name || 'tool' }), x(q.step.at), x(q.step.to)));
        } else {
          const ids = cluster.items.map((q) => q.step.id);
          const group = { ...first.step, to: lastItem.step.to, cluster: cluster.items.map((q) => q.step) };
          const b = stepButton(first.step, 'tj-tool tj-cluster', h('span', { class: 'tj-seg-text', text: `${cluster.items.length} calls` }), x(first.step.at), x(lastItem.step.to), {
            onclick: () => select(group),
            'aria-label': `${cluster.items.length} tool calls from ${clockOf(first.step.at)}; zoom in to separate them. First: ${first.step.name || 'tool'}.`,
            title: `${cluster.items.length} tool calls: ${cluster.items.slice(0, 3).map((q) => q.step.name).join(', ')}${cluster.items.length > 3 ? ', …' : ''}`,
            'data-cluster': ids.join(','),
          });
          track.append(b);
        }
        cluster = null;
      };
      for (const step of steps) {
        if (['tool'].includes(step.type)) {
          const w = x(step.to) - x(step.at);
          const near = cluster && x(step.at) - x(cluster.items[cluster.items.length - 1].step.to) < 2;
          if (w < MIN_PX && (near || !cluster)) {
            if (!cluster) cluster = { items: [] };
            cluster.items.push({ step });
            continue;
          }
          flush();
          track.append(stepButton(step, 'tj-tool', h('span', { class: 'tj-seg-text', text: step.name || 'tool' }), x(step.at), x(step.to)));
          continue;
        }
        flush();
        if (['model', 'wait', 'stall'].includes(step.type)) {
          const text = step.type === 'wait' ? (step.open ? '⏸ waiting' : (step.seg.decision && step.seg.decision.ok === false ? '✕ denied' : '⏸ waited')) : step.type === 'stall' ? '… stalled' : '';
          track.append(stepButton(step, kinds[step.type] + (step.open ? ' tj-open' : '') + (step.type === 'wait' && step.seg.decision && step.seg.decision.ok === false ? ' tj-bad' : ''),
            h('span', { class: 'tj-seg-text', text }), x(step.at), x(step.to)));
        } else if (step.type === 'prompt') {
          const b = stepButton(step, 'tj-point tj-prompt', h('span', { class: 'tj-glyph', 'aria-hidden': 'true', text: GLYPH.prompt }), x(step.at), x(step.at));
          b.style.width = '';
          track.append(b);
        } else if (POINTS.has(step.type) || step.type === 'end') {
          const refused = step.type === 'steer_rejected' || (step.type === 'delegation' && step.mark.ok === false) || (step.type === 'end' && step.bad);
          const glyph = step.type === 'end' ? outcomeGlyph(step.outcome) : (step.type === 'delegation' && step.mark.ok === false ? '✕' : GLYPH[step.type]);
          const b = stepButton(step, `tj-point tj-${step.type}${refused ? ' tj-bad' : ''}`, h('span', { class: 'tj-glyph', 'aria-hidden': 'true', text: glyph }), x(step.at), x(step.at));
          b.style.width = '';
          track.append(b);
        }
      }
      flush();
      if (turn.end == null) track.append(h('i', { class: 'tj-head', 'aria-hidden': 'true' }));
      if (track.lastChild && turn.end == null) track.lastChild.style.left = `${x(end)}px`;
      const row = h('div', { class: `tj-row tj-turn${bad ? ' tj-turn-bad' : ''}`, 'data-turn': String(turn.n), 'data-lane': lane.name }, label, track);
      return row;
    }

    // ----- steps: names and selection -----
    function stepLabel(lane, turn, step, total) {
      const dur = step.to > step.at ? `, ${fmtDur(step.to - step.at)}` : '';
      const where = `${lane.name === opts.branch ? '' : `${lane.name}, `}turn ${turn.n}, step ${step.index + 1} of ${total}`;
      const at = clockOf(step.at);
      switch (step.type) {
        case 'prompt': return `Prompt starts turn ${turn.n}, ${at}, ${where}`;
        case 'startup': return `Startup on ${turn.startup.provider}, ${PATH_WORDS[turn.startup.path] || turn.startup.path}, ${fmtDur(startupTotal(turn.startup))}, ${where}`;
        case 'model': return `Model working${dur}, ${at}, ${where}`;
        case 'tool': return `Tool call ${step.name}${dur}, ${at}, ${where}`;
        case 'wait': return step.open ? `Waiting for a person to answer ${step.name}${dur}, still open, ${where}` : `Waited for a person on ${step.name}${dur}, ${step.seg.decision ? (step.seg.decision.ok ? 'allowed' : 'denied') : 'no answer recorded'}, ${where}`;
        case 'stall': return `Stalled, no harness activity${dur}, ${where}`;
        case 'steer': return `A person steered the turn, ${at}, ${where}`;
        case 'steer_rejected': return `The steer was refused or dropped, ${at}, ${where}`;
        case 'delegation': return step.mark.ok === false ? `Delegation refused, ${at}, ${where}` : `Delegated to ${step.name || 'a child'}, ${at}, ${where}`;
        case 'judged': return `Judged ${Math.round(step.mark.score)} of 100, ${at}, ${where}`;
        case 'warning': return `Warning, ${at}, ${where}`;
        case 'end': return `Turn ended, ${outcomeWord(step.outcome)}, ${at}, ${where}`;
        default: return `${step.type}, ${where}`;
      }
    }

    function onKey(ev) {
      const buttons = Array.from(ui.scroll.querySelectorAll('[data-step]'));
      const at = buttons.indexOf(document.activeElement);
      const go = (i) => { const b = buttons[Math.max(0, Math.min(buttons.length - 1, i))]; if (b) { for (const o of buttons) o.tabIndex = o === b ? 0 : -1; view.focusId = b.dataset.step; b.focus(); if (b.scrollIntoView) b.scrollIntoView({ inline: 'nearest', block: 'nearest' }); } };
      const rowOf = (b) => b.closest('.tj-row');
      const rowsWith = Array.from(ui.scroll.querySelectorAll('.tj-row')).filter((r) => r.querySelector('[data-step]'));
      switch (ev.key) {
        case 'ArrowRight': ev.preventDefault(); go(at < 0 ? 0 : at + 1); break;
        case 'ArrowLeft': ev.preventDefault(); go(at < 0 ? 0 : at - 1); break;
        case 'Home': ev.preventDefault(); go(0); break;
        case 'End': ev.preventDefault(); go(buttons.length - 1); break;
        case 'ArrowDown': case 'ArrowUp': {
          if (at < 0) break;
          ev.preventDefault();
          const i = rowsWith.indexOf(rowOf(buttons[at]));
          const row = rowsWith[i + (ev.key === 'ArrowDown' ? 1 : -1)];
          const target = row && row.querySelector('[data-step]');
          if (target) go(buttons.indexOf(target));
          break;
        }
        case '+': case '=': ev.preventDefault(); zoom(1.6); break;
        case '-': case '_': ev.preventDefault(); zoom(1 / 1.6); break;
        case '0': ev.preventDefault(); view.zoom = 1; schedule(); break;
        case 'Escape': if (view.selected) { ev.preventDefault(); view.selected = null; view.detail = null; schedule(); } break;
        default:
      }
    }

    // ----- detail -----
    function renderDetailEmpty() {
      if (view.detail) return;
      ui.detail.replaceChildren(h('h2', { class: 'tj-detail-title', text: 'Step detail' }),
        h('p', { class: 'muted', text: 'Select a step to read what happened in it. The overview holds no conversation; a step reads only its own events.' }));
    }

    async function select(step) {
      view.selected = step.id;
      view.focusId = step.id;
      view.detail = { id: step.id };
      for (const b of ui.scroll.querySelectorAll('[data-step]')) b.setAttribute('aria-pressed', String(b.dataset.step === step.id));
      const title = h('h2', { class: 'tj-detail-title', tabindex: '-1', text: titleOf(step) });
      const body = h('div', { class: 'tj-detail-body' }, h('p', { class: 'muted', role: 'status', text: 'Reading the events…' }));
      ui.detail.replaceChildren(title, metaOf(step), body);
      ui.detail.setAttribute('data-step', step.id);
      // Under the chart on a narrow screen: bring it into view.
      if (root.innerWidth && root.innerWidth < 1100 && ui.detail.scrollIntoView) ui.detail.scrollIntoView({ block: 'nearest' });
      try {
        const content = await detailOf(step);
        if (view.detail && view.detail.id === step.id) body.replaceChildren(...content);
      } catch (e) {
        if (view.detail && view.detail.id === step.id) body.replaceChildren(h('p', { class: 'error', role: 'alert', text: `The events could not be read: ${e && e.message ? e.message : 'unknown error'}` }));
      }
    }
    function titleOf(step) {
      if (step.cluster) return `${step.cluster.length} tool calls: ${step.cluster.slice(0, 4).map((q) => q.name).join(', ')}${step.cluster.length > 4 ? ', …' : ''}`;
      const base = KIND_WORDS[step.type] || (step.type === 'child-turn' ? `${step.lane} turn ${step.turn.n}` : step.type);
      if (step.type === 'tool') return `Tool call: ${step.name}`;
      if (step.type === 'wait') return `Waiting for a person: ${step.name || 'a tool'}`;
      if (step.type === 'delegation') return step.mark.ok === false ? 'Delegation refused' : `Delegated to ${step.name || 'a child'}`;
      if (step.type === 'end') return `Turn ${step.turn.n} ended: ${outcomeWord(step.outcome)}`;
      if (step.type === 'child-turn') return `${step.lane}, turn ${step.turn.n}`;
      return base;
    }
    function metaOf(step) {
      const items = [['Branch', step.lane], ['Turn', String(step.turn.n)], ['At', clockOf(step.at)]];
      if (step.to > step.at) items.push(['Lasted', fmtDur(step.to - step.at)]);
      if (step.seq) items.push(['Event', `#${step.seq}`]);
      return h('dl', { class: 'tj-detail-meta' }, items.map(([k, v]) => h('div', {}, h('dt', { text: k }), h('dd', { text: v }))));
    }

    // The events a step covers: from its own number up to the next step's,
    // or located by time when the step came from the stream and has no
    // number yet.
    async function eventsFor(step) {
      let seq = step.seq;
      if (!seq) seq = await locate(step.lane, step.at);
      if (!seq) return { events: [], seq: 0 };
      let limit = 1;
      if (step.type === 'model' || step.type === 'tool' || step.type === 'end' || step.type === 'wait' || step.type === 'child-turn') limit = 30;
      if (step.type === 'tool' || step.type === 'model') limit = 40;
      if (step.cluster) limit = PAGE;
      if (step.type === 'wait') limit = 6;
      if (step.type === 'end') limit = 6;
      const page = await opts.api(`${opts.path(step.lane)}/event-page?cursor=${seq - 1}&limit=${Math.min(PAGE, limit)}`);
      return { events: page.events || [], seq, total: page.total };
    }
    // The number of the event recorded at `at`, by reading back from the
    // newest page until a page reaches it.
    async function locate(name, at) {
      let page = await opts.api(`${opts.path(name)}/event-page?limit=${PAGE}`);
      for (let guard = 0; guard < 80; guard++) {
        const events = page.events || [];
        if (!events.length) return 0;
        const firstNo = page.next_cursor - events.length + 1;
        if (events[0].at_ms < at || firstNo <= 1) {
          const idx = events.findIndex((e) => e.at_ms >= at);
          return idx < 0 ? 0 : firstNo + idx;
        }
        const cursor = Math.max(0, firstNo - 1 - PAGE);
        page = await opts.api(`${opts.path(name)}/event-page?cursor=${cursor}&limit=${firstNo - 1 - cursor}`);
      }
      return 0;
    }

    async function detailOf(step) {
      if (step.type === 'startup') return startupDetail(step);
      if (step.type === 'child-turn') return childDetail(step);
      const { events, seq } = await eventsFor(step);
      return eventsDetail(step, events, seq);
    }
    function startupDetail(step) {
      const st = step.startup;
      const rows = phaseDurations(st);
      return [
        h('p', { text: `${st.provider} · ${PATH_WORDS[st.path] || st.path}. Phases are counted from the operation's admission; the harness's first event came ${fmtDur(startupTotal(st))} after it.` }),
        h('table', { class: 'tj-table' }, h('thead', {}, h('tr', {}, h('th', { text: 'Phase' }), h('th', { text: 'Took' }), h('th', { text: 'From admission' }))),
          h('tbody', {}, rows.map((p) => h('tr', {}, h('td', { text: p.word }), h('td', { text: fmtDur(p.ms) }), h('td', { text: fmtDur(p.offset) }))))),
        h('p', { class: 'hint', text: 'This is read from the timeline; there is no separate event to open.' }),
      ];
    }
    function childDetail(step) {
      const turn = step.turn;
      return [
        h('p', { text: `${plural(turn.tools.length, 'tool call')}, ${outcomeWord(turn.end == null ? null : turn.outcome)}${turn.cost != null ? `, ${cents(turn.cost)}` : ''}.` }),
        h('p', {}, h('a', { href: opts.href.trajectory(step.lane), text: `Open ${step.lane}'s trajectory` }), ' · ', h('a', { href: opts.href.branch(step.lane), text: 'Open the branch' })),
        h('p', { class: 'hint', text: 'Expand the lane to see this turn step by step.' }),
      ];
    }

    function activityText(a) {
      if (a == null) return '';
      if (typeof a === 'string') return a;
      const [kind] = Object.keys(a);
      const v = a[kind];
      switch (kind) {
        case 'prompt': return v;
        case 'steered': return `${v.by}: ${v.text}`;
        case 'decision': return `${v.allowed ? 'Allowed' : 'Denied'} ${v.tool}${v.message ? `: ${v.message}` : ''} (${v.source && typeof v.source === 'object' ? Object.keys(v.source)[0] : v.source})`;
        case 'warning': return v;
        case 'status': return `status: ${outcomeWord(v)}${v.reason ? `: ${v.reason}` : ''}`;
        case 'turn_cost': return `turn cost ${cents(v.cost_usd)}`;
        case 'delegation': return `${v.tool} ${v.branch || ''}: ${v.outcome}${v.refused ? ' (refused)' : ''}`;
        case 'stalled': return 'no harness activity for the stall window';
        case 'fleet': return `${v.type}${v.score != null ? ` ${Math.round(v.score)}/100` : ''}${v.reason ? `: ${v.reason}` : ''}`;
        case 'harness': return harnessText(v);
        default: return opts.describe ? opts.describe(a) : kind.replace(/_/g, ' ');
      }
    }
    function harnessText(v) {
      switch (v.type) {
        case 'tool_started': return `tool ${v.name} (call ${v.call_id})`;
        case 'message_delta': return v.text;
        case 'permission_requested': return `asks to use ${v.request.tool}: ${clip(JSON.stringify(v.request.input), 600)}`;
        case 'turn_ended': return `turn ended: ${v.outcome && v.outcome.kind}${v.outcome && v.outcome.message ? `: ${v.outcome.message}` : ''}`;
        case 'usage_observed': return `usage: ${v.usage.input_tokens == null ? '?' : v.usage.input_tokens} in, ${v.usage.output_tokens == null ? '?' : v.usage.output_tokens} out${v.usage.cost_usd != null ? `, ${cents(v.usage.cost_usd)}` : ''}`;
        case 'steer_rejected': return `steer refused: ${v.reason}`;
        case 'warning': return `warning: ${v.message}`;
        case 'open_failed': return `opening failed: ${v.reason}`;
        case 'outcome_unknown': return `outcome unknown: ${v.reason}`;
        default: return v.type.replace(/_/g, ' ');
      }
    }
    const clip = (s, n) => (s && s.length > n ? `${s.slice(0, n - 1)}…` : s || '');

    function eventsDetail(step, events, seq) {
      const out = [];
      if (!events.length) return [h('p', { class: 'muted', text: 'No events were found for this step.' })];
      // Message deltas arrive a few words at a time: join each run.
      const lines = [];
      for (const e of events) {
        const delta = e.activity && e.activity.harness && e.activity.harness.type === 'message_delta';
        const prev = lines[lines.length - 1];
        if (delta && prev && prev.delta) { prev.text += e.activity.harness.text; continue; }
        lines.push({ at: e.at_ms, delta, text: activityText(e.activity), raw: e });
      }
      // For a step that is one event, show the events up to the next
      // step's first only (a tool call reads its own span).
      const bound = step.to > step.at ? step.to : step.at;
      const shownLines = (step.type === 'tool' || step.type === 'model' || step.type === 'wait') ? lines.filter((l) => l.at <= bound + 1 || l === lines[0]) : lines;
      const useLines = shownLines.length ? shownLines : lines.slice(0, 1);
      if (step.type === 'wait') out.push(h('p', { class: 'hint', text: 'The request, then the answer that ended the wait.' }));
      out.push(h('ol', { class: 'tj-events' }, useLines.map((l) => h('li', {}, h('time', { datetime: new Date(l.at).toISOString(), text: clockOf(l.at) }), h('span', { class: l.delta ? 'tj-said' : '', text: clip(l.text, 4000) })))));
      out.push(h('details', { class: 'tj-raw' }, h('summary', { text: 'Raw events' }), h('pre', { class: 'mono', text: clip(JSON.stringify(events, null, 1), 20000) })));
      if (step.type === 'delegation' && step.name) out.push(h('p', {}, h('a', { href: opts.href.trajectory(step.name), text: `Open ${step.name}'s trajectory` })));
      out.push(h('p', { class: 'hint' }, `Events ${seq}–${seq + events.length - 1} of the branch's log. `, h('a', { href: opts.href.branch(step.lane), text: 'Open the branch for its Activity' })));
      return out;
    }

    // ----- compare -----
    function renderCompare() {
      const group = compareSet().names;
      if (!view.compare || group.length < 2) { ui.compare.hidden = true; ui.compare.replaceChildren(); return; }
      ui.compare.hidden = false;
      const t = now();
      const rows = group.map((name) => {
        const e = view.lanes.get(name);
        const info = branches().get(name);
        const lane = e && e.lane;
        const sum = lane ? summarize(lane, t) : null;
        const last = lane && lane.turns[lane.turns.length - 1];
        return { name, info, lane, sum, score: lane ? judgeOf(lane) : null, outcome: last ? (last.end == null ? null : last.outcome) : null };
      });
      rows.sort((a, b) => (b.score == null ? -1 : b.score) - (a.score == null ? -1 : a.score));
      const longest = Math.max(1, ...rows.map((r) => (r.lane && r.lane.turns.length ? (r.sum.last - r.sum.first) : 0)));
      const attempt = (r) => {
        const bar = h('div', { class: 'tj-cmp-bar', role: 'img', 'aria-label': r.lane ? `${r.name}: ${fmtDur(r.sum.last - r.sum.first)} from its first turn` : `${r.name}: not read yet` });
        if (r.lane) {
          for (const turn of r.lane.turns) {
            const end = turn.end != null ? turn.end : t;
            const seg = h('i', { class: `tj-cmp-turn${turn.outcome && BAD.has(turn.outcome.state) ? ' tj-bad' : ''}` });
            seg.style.left = `${((turn.start - r.sum.first) / longest) * 100}%`;
            seg.style.width = `${Math.max(0.6, ((end - turn.start) / longest) * 100)}%`;
            bar.append(seg);
            for (const tool of turn.tools.length < 200 ? turn.tools : []) {
              const tick = h('i', { class: 'tj-cmp-call' });
              tick.style.left = `${((tool.at - r.sum.first) / longest) * 100}%`;
              bar.append(tick);
            }
          }
        }
        return bar;
      };
      ui.compare.replaceChildren(
        h('h2', { text: `Compare ${rows.length} ${compareSet().word}` }),
        h('p', { class: 'hint', text: 'Each attempt on one clock started at its own first turn. The score is the judge\'s, when one has scored it.' }),
        h('div', { class: 'tj-cmp-scroll' }, h('table', { class: 'tj-table tj-cmp' },
          h('thead', {}, h('tr', {}, ['Attempt', 'Harness and model', 'Outcome', 'Score', 'Time', 'Calls', 'Tokens', 'Cost', 'Timeline'].map((c) => h('th', { scope: 'col', text: c })))),
          h('tbody', {}, rows.map((r) => h('tr', { 'data-attempt': r.name },
            h('th', { scope: 'row' }, h('a', { href: opts.href.trajectory(r.name), text: r.name })),
            h('td', { text: [r.info && r.info.harness, r.info && r.info.model].filter(Boolean).join(' · ') || '—' }),
            h('td', { text: r.lane ? `${outcomeGlyph(r.outcome)} ${outcomeWord(r.outcome)}` : '…' }),
            h('td', { class: 'tj-num', text: r.score == null ? 'not scored' : String(Math.round(r.score)) }),
            h('td', { class: 'tj-num', text: r.sum ? fmtDur(r.sum.span) : '…' }),
            h('td', { class: 'tj-num', text: r.sum ? String(r.sum.calls) : '…' }),
            h('td', { class: 'tj-num', text: r.sum ? fmtTokens(r.sum.tokens) : '…' }),
            h('td', { class: 'tj-num', text: cents(r.info && r.info.cost_usd != null ? r.info.cost_usd : r.sum && r.sum.cost) }),
            h('td', { class: 'tj-cmp-cell' }, attempt(r))))))));
    }

    // ----- the list -----
    function renderList() {
      const body = ui.list.querySelector('.tj-listing-body');
      const entry = view.lanes.get(opts.branch);
      if (!entry || !entry.lane) { body.replaceChildren(); return; }
      const t = now();
      const start = entry.lane.turns.length ? entry.lane.turns[0].start : t;
      const items = [];
      for (const turn of entry.lane.turns) {
        const { steps } = stepsOf(entry.lane, turn, t);
        for (const step of steps) items.push(h('li', { text: `+${fmtDur(step.at - start)} · ${stepLabel(entry.lane, turn, step, steps.length)}` }));
      }
      body.replaceChildren(h('ol', { class: 'tj-steps' }, items.slice(0, 400)), items.length > 400 ? h('p', { class: 'hint', text: `${items.length - 400} more steps are not listed.` }) : null);
    }

    build();
    if (typeof CSS === 'undefined' || !CSS.escape) { /* very old browsers: focus restore is skipped */ }
    for (const s of shown()) ensureLane(s.name);
    render();
    return {
      activity,
      branchesChanged,
      refresh: schedule,
      destroy() { view.destroyed = true; clearTimeout(view.timer); if (ui.resize) ui.resize.disconnect(); container.replaceChildren(); },
      // For tests.
      state: view,
    };
  }

  const api = {
    mount,
    model: { laneOf, fold, stepsOf, split, summarize, buildScale, ticksOf, niceStep, fanGroup, fmtDur, fmtTokens, phaseDurations, startupTotal },
  };
  root.BranchyardTrajectory = api;
  if (typeof module !== 'undefined' && module.exports) module.exports = api;
})(typeof window !== 'undefined' ? window : globalThis);
