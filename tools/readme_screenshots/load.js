// The browser half of load.py: open the console's home page while the demo's branches run,
// count what it does for SECONDS, then stop every running branch and compare each board row with
// the server's timeline. Prints one JSON object. Reads BY_URL, BY_TOKEN, BY_REPO, SECONDS and
// PLAYWRIGHT from the environment.
const { chromium } = require(process.env.PLAYWRIGHT);

const app = `${process.env.BY_URL}/app/`;
const repo = process.env.BY_REPO;
const seconds = Number(process.env.SECONDS || 40);

(async () => {
  const browser = await chromium.launch();
  try {
    const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
    await page.goto(app);
    await page.getByLabel('Server token', { exact: true }).fill(process.env.BY_TOKEN);
    await page.keyboard.press('Enter');
    await page.locator('#main .board-row').first().waitFor({ timeout: 60000 });

    const requests = [];
    let watching = true;
    page.on('response', async (response) => {
      if (!watching) return;
      const url = new URL(response.url());
      if (!url.pathname.startsWith('/v1/') || url.pathname.endsWith('/stream')) return;
      let bytes = 0;
      try { bytes = (await response.body()).length; } catch (_) { /* a body the page dropped */ }
      requests.push({ path: url.pathname.replace(/branches\/[^/]+/, 'branches/{b}'), bytes });
    });
    await page.evaluate(() => {
      window.loadCounts = { renders: 0, longtasks: 0, long_ms: 0 };
      new MutationObserver((records) => {
        for (const r of records) if (r.target.id === 'main') window.loadCounts.renders += 1;
      }).observe(document.getElementById('main'), { childList: true });
      new PerformanceObserver((list) => {
        for (const e of list.getEntries()) { window.loadCounts.longtasks += 1; window.loadCounts.long_ms += e.duration; }
      }).observe({ type: 'longtask' });
    });
    const t0 = Date.now();
    // The measurement is the page's behaviour over a fixed window, not a wait for a condition.
    await page.waitForTimeout(seconds * 1000);
    const t1 = Date.now();
    watching = false;
    const perf = await page.evaluate(() => window.loadCounts);

    const rows = await page.evaluate(async ({ repo, token }) => {
      const call = (method, path) => fetch(path, {
        method, headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' }, body: method === 'POST' ? '{}' : undefined,
      }).then((r) => r.json());
      const base = `/v1/repos/${encodeURIComponent(repo)}/branches`;
      const listing = await call('GET', base);
      const branches = Array.isArray(listing) ? listing : listing.branches;
      for (const b of branches) if (b.status.state === 'running') await call('POST', `${base}/${encodeURIComponent(b.name)}/cancel`);
      const shape = async (name) => {
        const t = await call('GET', `${base}/${encodeURIComponent(name)}/timeline`);
        // A server older than the timeline has nothing to compare with.
        if (!Array.isArray(t.turns)) return null;
        return t.turns.map((x) => [(x.tools || []).length, x.ended_at_ms != null]);
      };
      const drawn = (name) => Array.from(document.querySelectorAll(`#main .board-row[data-branch="${CSS.escape(name)}"] .rail .seg`))
        .map((bar) => [Number((bar.title.match(/(\d+) calls?/) || [0, 0])[1]), !bar.title.includes('running')]);
      // Every branch has stopped; give the stream until each row agrees, or 20 s.
      const out = [];
      for (const b of branches) {
        const want = JSON.stringify(await shape(b.name));
        const deadline = Date.now() + 20000;
        let got = JSON.stringify(drawn(b.name));
        while (got !== want && Date.now() < deadline) {
          await new Promise((r) => requestAnimationFrame(r));
          got = JSON.stringify(drawn(b.name));
        }
        out.push({ branch: b.name, server: JSON.parse(want), drawn: JSON.parse(got), same: got === want });
      }
      return out;
    }, { repo, token: process.env.BY_TOKEN });

    const byPath = {};
    for (const r of requests) {
      byPath[r.path] = byPath[r.path] || { n: 0, bytes: 0 };
      byPath[r.path].n += 1;
      byPath[r.path].bytes += r.bytes;
    }
    console.log(JSON.stringify({
      window: [t0, t1], seconds: (t1 - t0) / 1000, perf,
      requests: requests.length, bytes: requests.reduce((n, r) => n + r.bytes, 0), by_path: byPath, rows,
    }));
  } finally {
    await browser.close();
  }
})();
