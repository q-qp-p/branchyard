// The Branchyard documentation site: search, copy buttons, the mobile menu and the
// "On this page" highlight. No dependencies. The site works without it.
(() => {
  "use strict";

  // Copy buttons on code blocks.
  document.addEventListener("click", async (event) => {
    const button = event.target.closest("[data-copy]");
    if (!button) return;
    const code = button.closest(".code")?.querySelector("pre code");
    if (!code) return;
    try {
      await navigator.clipboard.writeText(code.innerText.replace(/\n$/, ""));
      button.textContent = "Copied";
      button.classList.add("done");
    } catch {
      button.textContent = "Press Ctrl+C";
    }
    setTimeout(() => {
      button.textContent = "Copy";
      button.classList.remove("done");
    }, 1600);
  });

  // The menu on narrow screens.
  const menu = document.querySelector("[data-menu]");
  const nav = document.getElementById("nav");
  if (menu && nav) {
    const set = (open) => {
      nav.classList.toggle("open", open);
      menu.setAttribute("aria-expanded", String(open));
    };
    menu.addEventListener("click", () => set(!nav.classList.contains("open")));
    document.addEventListener("keydown", (e) => {
      if (e.key === "Escape" && nav.classList.contains("open")) {
        set(false);
        menu.focus();
      }
    });
    nav.querySelector('[aria-current="page"]')?.scrollIntoView({ block: "center" });
  }

  // "On this page": mark the section in view.
  const tocLinks = [...document.querySelectorAll(".toc a")];
  if (tocLinks.length && "IntersectionObserver" in window) {
    const byId = new Map(tocLinks.map((a) => [decodeURIComponent(a.hash.slice(1)), a]));
    const visible = new Set();
    const observer = new IntersectionObserver(
      (entries) => {
        for (const entry of entries) {
          if (entry.isIntersecting) visible.add(entry.target.id);
          else visible.delete(entry.target.id);
        }
        const first = [...byId.keys()].find((id) => visible.has(id));
        if (!first) return;
        tocLinks.forEach((a) => a.classList.toggle("active", a === byId.get(first)));
      },
      { rootMargin: "-64px 0px -60% 0px" },
    );
    for (const id of byId.keys()) {
      const heading = document.getElementById(id);
      if (heading) observer.observe(heading);
    }
  }

  // Search over search.json: titles first, then headings, then summaries.
  const box = document.querySelector("[data-search]");
  if (!box) return;
  const input = box.querySelector("input");
  const list = box.querySelector(".results");
  const prefix = box.dataset.prefix || "";
  let index = null;
  let selected = -1;

  const load = async () => {
    if (index) return index;
    const response = await fetch(prefix + "search.json");
    index = await response.json();
    return index;
  };

  const escape = (s) => s.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
  const highlight = (text, words) => {
    let out = escape(text);
    for (const w of words) {
      if (w.length < 2) continue;
      out = out.replace(new RegExp(`(${w.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")})`, "ig"), "<mark>$1</mark>");
    }
    return out;
  };

  // Every word must appear somewhere. A title match counts most, then a heading, then text.
  const search = (query) => {
    const words = query.toLowerCase().split(/\s+/).filter(Boolean);
    if (!words.length) return [];
    const hits = [];
    for (const page of index) {
      const title = page.t.toLowerCase();
      const summary = page.d.toLowerCase();
      const inTitle = words.filter((w) => title.includes(w)).length;
      // Pages kept for the record, or linked from nowhere but another page, rank below current guidance.
      const weight = page.s && page.s !== "History" ? 1 : 0.5;
      if (words.every((w) => title.includes(w) || summary.includes(w))) {
        hits.push({ page, score: weight * (inTitle * 10 + (title.startsWith(words[0]) ? 5 : 0) + 2), heading: null });
      }
      for (const [heading, id, text] of page.h) {
        const head = heading.toLowerCase();
        const body = text.toLowerCase();
        if (!words.every((w) => head.includes(w) || body.includes(w) || title.includes(w))) continue;
        const inHead = words.filter((w) => head.includes(w)).length;
        hits.push({ page, score: weight * (inHead * 6 + inTitle * 3 + 1), heading: [heading, id], text });
      }
    }
    const seen = new Set();
    return hits
      .sort((a, b) => b.score - a.score)
      .filter((hit) => {
        const key = hit.page.u + (hit.heading ? hit.heading[1] : "");
        return !seen.has(key) && seen.add(key);
      })
      .slice(0, 12);
  };

  // A short window of the section's text around the first word that matched.
  const excerpt = (text, words) => {
    const low = text.toLowerCase();
    const at = Math.max(0, Math.min(...words.map((w) => low.indexOf(w)).filter((i) => i >= 0), text.length));
    const start = Math.max(0, at - 40);
    return (start ? "…" : "") + text.slice(start, start + 140);
  };

  const render = (hits, words) => {
    selected = hits.length ? 0 : -1;
    list.innerHTML = hits.length
      ? hits
          .map((hit, i) => {
            const url = prefix + hit.page.u + (hit.heading ? "#" + hit.heading[1] : "");
            const title = hit.heading
              ? `${highlight(hit.heading[0], words)} <span class="h">· ${escape(hit.page.t)}</span>`
              : highlight(hit.page.t, words);
            const detail = hit.heading ? highlight(excerpt(hit.text || "", words), words) : escape(hit.page.d || "");
            return `<li role="presentation"><a role="option" id="r${i}" href="${url}" aria-selected="${i === 0}"><span class="t">${title}</span><span class="d">${detail}</span></a></li>`;
          })
          .join("")
      : '<li class="none">Nothing matches. Try fewer words.</li>';
    list.hidden = false;
    input.setAttribute("aria-expanded", "true");
    input.setAttribute("aria-activedescendant", hits.length ? "r0" : "");
  };

  const close = () => {
    list.hidden = true;
    input.setAttribute("aria-expanded", "false");
    input.removeAttribute("aria-activedescendant");
  };

  const move = (by) => {
    const options = [...list.querySelectorAll("[role=option]")];
    if (!options.length) return;
    selected = (selected + by + options.length) % options.length;
    options.forEach((o, i) => o.setAttribute("aria-selected", String(i === selected)));
    options[selected].scrollIntoView({ block: "nearest" });
    input.setAttribute("aria-activedescendant", options[selected].id);
  };

  input.addEventListener("focus", load, { once: true });
  input.addEventListener("input", async () => {
    const query = input.value.trim();
    if (!query) return close();
    await load();
    render(search(query), query.toLowerCase().split(/\s+/));
  });
  input.addEventListener("keydown", (e) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      move(1);
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      move(-1);
    } else if (e.key === "Enter") {
      const option = list.querySelectorAll("[role=option]")[selected];
      if (option) location.href = option.href;
    } else if (e.key === "Escape") {
      input.value = "";
      close();
      input.blur();
    }
  });
  document.addEventListener("click", (e) => {
    if (!box.contains(e.target)) close();
  });
  document.addEventListener("keydown", (e) => {
    const typing = /^(INPUT|TEXTAREA|SELECT)$/.test(document.activeElement?.tagName || "");
    if (e.key === "/" && !typing && !e.metaKey && !e.ctrlKey) {
      e.preventDefault();
      input.focus();
    }
  });
})();
