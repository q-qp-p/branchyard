# Dogfood round 6, feature 2: Office documents in a real viewer

Written 1 November 2026. The second feature of round 6: Branchyard building a Branchyard feature with real harnesses. The same task went verbatim to Claude Code on Anthropic, Claude Code on Z.ai's GLM, Codex on a ChatGPT plan login, Codex on GLM and Antigravity on Gemini 4 Argon, through `by fan` and `by run`. The attempts were watched in the console, judged by the deterministic judge and by Claude Code, compared, and tested one by one. The best attempt landed on the branch `office-viewer`. The first feature, an HTML artifact seen and used from the console, is recorded in the round's `README.md` beside this page (committed on the branch `dogfood-round6`), together with the four drivers' live qualification.

Files beside this page: the screenshots `office-viewer-*.png`, 1440 and 390 px wide. The board and the deck come from the scratch yard while the attempts ran. The viewer pictures come from the landed feature's browser test with the real converter.

## What ran

Everything ran in one Linux container (kernel 6.18.44, four cores) on 1 November 2026, between 07:50 and 09:45 UTC. `by` was 0.3.1, the first agent's build of the same Rust tree. This branch's base, `5eb5371f`, differs from that build's base `7746ef1e` only in a CI workflow and a Python test. Each attempt worked in its own worktree of a scratch clone of this branch.

| Harness | Version | Model | Credentials |
|---|---|---|---|
| Claude Code | 2.1.316 | `claude-sonnet-5-5` (its default; named in the session transcript) | the environment's sign-in, reaching a private home (`--isolated`) |
| Claude Code on Z.ai | 2.1.316 | `glm-5.3` (`--model`) | a private user configuration whose `[logins]` names `zai` and whose `[secrets]` names the variable holding the key, not the key |
| Codex | codex-cli 0.166.0 | `gpt-6.1-sol` under the person's ChatGPT plan login (read from the session's rollout file) | the signed-in home as the harness's `HOME`, in local mode; nothing copied or changed in it |
| Codex on Z.ai | codex-cli 0.166.0 | `glm-5.3` | the same private user configuration as Claude Code on Z.ai; Branchyard writes the `zai` provider into the private home's `config.toml` |
| Antigravity | 1.4.12, unchanged during the run (the binary fetched pinned at 1.4.9 had updated itself earlier in the day) | `gemini-4-argon-high`, passed by a two-line wrapper named `agy` on `PATH`, because `by fan` takes one `--model` for every harness | `GEMINI_API_KEY` as a `--secret` |

**The task.** "Build this Branchyard feature in this repository: Office documents (DOCX, PPTX, XLSX) open and render properly in the console". The prompt gave the five acceptance points below verbatim as the check. It also gave the repository's build rules for parallel worktrees: one shared, warm target directory under one lock, `CARGO_HOME` for the offline registry, and the converter's variables. It named the order to work in (page path, viewer, grid) and asked for a list of what could not be done and the friction met.

1. **Every page and slide.** A DOCX or PPTX output or source renders all its pages or slides, each fetched on demand (`…/preview?view=pages&page=N`, with the page count in the first answer). They are rendered by the same sandboxed LibreOffice and Poppler path and cached per artifact and page. A documented page cap (64) bounds them, with a clear message past it.
2. **A real document viewer.** "Open" on a document goes to a full-width viewer in the console (`#/w/<workspace>/doc/<artifact>`). Pages are stacked and scrollable with the current page number shown, or, for slides, shown one at a time with a thumbnail strip. The viewer has previous and next by keys and buttons, fit-to-width zoom, Download original, Accept where the artifact is an unaccepted output, and Escape back. It works the same at 390 px, with the thumbnails folded.
3. **Spreadsheets as spreadsheets.** An XLSX opens with a tab per sheet and a grid that keeps the sheet's column widths, merged cells, number and date formats and frozen header row. The grid is paged by 200 rows with the range shown, and formulas are shown as values with the formula on hover. A "print view" toggle shows LibreOffice's rendering of the sheet through the pages path.
4. **Hermetic tests.** Python: `tests/test_artifact_previews.py` with generated fixtures (python-docx 3 pages, python-pptx 3 slides, openpyxl 2 sheets with a merged cell and a frozen row) and a fake renderer for the page path. Rust: the preview service tests, with the recording fake namespace command for per-page requests and the cache. Browser: a step in `console_workspace_browser.{rs,js}` that opens each kind in the viewer and navigates. It passes with the real converter when `BRANCHYARD_PREVIEW_BWRAP` and `BRANCHYARD_PREVIEW_PYTHON` are set, and asserts the "cannot make previews" state when they are not. `every_asset_is_self_contained` and the CSP tests stay green.
5. **Docs and gates.** `docs/artifact-previews.md`, `docs/artifact-preview-component.md`, `docs/companion.md` (a dated status paragraph with evidence and "Not done:"), and CHANGELOG.md Unreleased. The gates: `python3 tools/check.py --fast`, `cargo clippy -p branchyard-server --all-targets -- -D warnings`, `cargo test -p branchyard-server`.

**The runs.** All five attempts ran from the scratch clone with `--permissions full --check "python3 tools/check.py --fast" --budget-usd 8 --max-minutes 60`:

- `by fan "<task>" --harness claude-code,antigravity --isolated --allow-unapproved-tools --secret GEMINI_API_KEY --name docviewer --json`.
- `by run` for Claude Code on GLM (`--harness claude-code --model glm-5.3 --isolated --name docviewer-glm`).
- `by run` for Codex on GLM (the same with `--harness codex`; it needs `--no-preflight`, see below).
- `by run` for Codex on the ChatGPT login (`--harness codex`, no `--isolated`, `HOME` set to the signed-in home).

Codex on its plan login cannot share the fan with the isolated harnesses: a fan has one `HOME` policy for all its attempts.

All five started between 07:50 and 07:51 and ran beside three attempts of the first feature, so **eight real agents ran at once** on four cores. Every Cargo build of every attempt queued on one lock. The load average was 7.76 (1 minute), 11.54 (5 minutes) and 9.87 (15 minutes) at 08:07. The build rules were in the prompt only, not in the attempts' environment. The integrator's later rule (`CARGO_BUILD_JOBS=2` and the target directory in each attempt's environment, a commit after each passing unit, no new attempt while four or more run) came after these five started. No attempt was started after it.

## Result

Costs are what `by ls --json` records (`cost_usd`); `-` means a harness that reports none. "Check" is the attempt's `--check` as both judges ran it on its exact candidate. "Tests" is what I ran on each candidate as it stood, under the attempts' shared lock and target directory: `python3 tools/check.py --fast`, `cargo fmt --check`, `cargo clippy -p branchyard-server --all-targets -- -D warnings`, and `cargo test -p branchyard-server` with the browser tests, a real Bubblewrap 0.9.0, LibreOffice 24.2.7.2 and Poppler 24.02.0, and Chromium Headless Shell 153. The deterministic judge scores 0 for an attempt whose last turn did not complete.

| Attempt | Harness | Model | Outcome | Turns | Wall time | Cost | Check | Tests | Deterministic | Claude Code judge |
|---|---|---|---|---|---|---|---|---|---|---|
| `docviewer-codex-openai` | Codex 0.166.0 | `gpt-6.1-sol` (ChatGPT plan) | **ready**, 27 files +2254 −183 | 1 | 56 min | - (29.7M tokens, mostly cached) | passed | **all pass**: fast PASSED, fmt and clippy clean, 443 passed, 0 failed, 1 ignored; the browser test printed `DOCX output: pages; PPTX output: pages` | **95**, the proposed pick | **68**, ranked first, the proposed pick |
| `docviewer-claude-code` | Claude Code 2.1.316 | `claude-sonnet-5-5` | over budget (max_duration), 20 files +1758 −85 | 1 | 59 min | $2.28 | passed | fast PASSED, clippy clean, 442 passed, 0 failed with the real converter; **`cargo fmt --check` fails** on `preview/tests.rs` | 0 (not eligible) | 62 |
| `docviewer-antigravity` | Antigravity 1.4.12 | `gemini-4-argon-high` | over budget; "outcome unknown: the harness connection closed before the turn ended", 27 files +2971 −172 | 1 | 59 min | - | passed | fast PASSED, clippy clean; 441 passed, **1 failed**: its own viewer step in the workspace browser test, `the slide viewer at 390px overflows at 390px`; `cargo fmt --check` fails on `preview/tests.rs` | 0 (not eligible) | 55 |
| `docviewer-glm` | Claude Code 2.1.316 | `glm-5.3` | **ready**, 18 files +1868 −238 | 1 | 27 min | $7.26 (Claude Code's price; Z.ai bills the plan) | failed (`ruff_check`: E501 in `tools/console_fixtures.py`) | fast FAILED in 8 steps (ruff, mypy, `check_silent_failures`, `artifact_assets`, `docs_facts`); clippy and the tests do not compile: five `no method named clone found for struct PreviewInput` in its own new test, and a redundant closure | 0 (check failed) | 42 |
| `docviewer-codex-glm` | Codex 0.166.0 | `glm-5.3` | over budget, 19 files +1366 −122 | 1 | 59 min | - (13.9M tokens) | failed (`artifact_assets`: the embedded helper copies under `plugins/branchyard/skills/artifact-tools/scripts/` were not regenerated) | fast FAILED (`artifact_assets` only); `cargo fmt --check` fails; clippy and the tests do not compile: `cannot find value PAGE in this scope` in its new preview test | 0 | 35 |

**Both judges proposed the Codex attempt on the ChatGPT plan, and it is the pick.** It was cherry-picked onto `office-viewer`, cut from this branch, with its commit's author kept. The message names the harness, because the author line is the scratch clone's. A second commit replaces two machine paths in its evidence record with placeholders. Nothing else changed. On that branch:

- `python3 tools/check.py --fast` PASSED.
- `cargo fmt --check`, `cargo clippy -p branchyard-server --all-targets -- -D warnings`, and `cargo doc -p branchyard-server --no-deps` with `RUSTDOCFLAGS="-D warnings"` were clean.
- `cargo test -p branchyard-server` passed 443 tests in 35 binaries (0 failed, 1 ignored) with no preview renderer configured. `every_asset_is_self_contained` and the CSP tests were included, and the workspace browser test saw "renderer unavailable" for every format and asserted the viewer's "This server cannot make previews right now".
- The workspace browser test passed again with Bubblewrap and the preview Python set. It printed `DOCX output: pages; PPTX output: pages` and took the viewer pictures beside this page.

Neither judge could rank on equal terms. Only two of the five turns completed (the Codex attempt and Claude Code on GLM). The 60-minute cap ended the other three as `over budget (max_duration)`, and a judge scores such an attempt as not pickable, whatever its tests say. The Claude Code attempt and the Antigravity attempt passed the check, but neither was eligible. The pick stands on its own evidence: it is the only candidate that met all five points with every gate green as it stood. The Claude Code attempt came closest, with the real-converter browser test passing and only formatting left.

What each one got right and wrong, from the diffs and the test runs:

- **Codex (ChatGPT plan).** Everything in the list, with depth:
  - The server refuses `page` past 64 with "document previews are limited to pages 1 through 64; download the original for later pages". A longer original renders its first 64 with a warning.
  - DOCX pages are placeholders with the page's aspect ratio, loaded by an `IntersectionObserver` through one serial queue, so no request storm.
  - XLSX column widths, merges and the frozen pane are read from the sheet XML by `iterparse`. Excel number and date format codes are applied in Python, with the formula kept beside the cached value. A cell without a cached value gets one bounded LibreOffice recalculation of a private copy.
  - Print View hides the other sheets in a private staging copy, so only the selected sheet prints.
  - A test checks that the embedded helper copies match the SDK.
  - A dated evidence record (`docs/evidence/console-office-viewer-2026-11-01.json`) and fact-region docs.
  - Weak points: it reads openpyxl's private `_archive` and `_worksheet_path`. "Open" sits only in the preview panel's header, not on each list row. The evidence record named this machine's paths (replaced on landing).
- **Claude Code (Anthropic).** A sound viewer and a separate `xlsx_grid.py` module, with the page cap and per-page tests. It mirrored the helper into the skill and registered it in `branchyard-provision`. The cap stopped it while it re-ran the real-converter browser test, which then passed in my run. Only `cargo fmt` fails.
- **Antigravity.** The largest diff: a viewer, a grid, a print toggle, an Accept flow, and edits to `docx.py` and `xlsx.py` beyond the task. Its check passed and clippy was clean, but its own browser step found the slide viewer overflowing at 390 px, and `cargo fmt` fails. It never wrote a closing message.
- **Claude Code on GLM.** The fastest (27 minutes) and the only other completed turn. It has the best-separated backend: bounded validation of the new fields in Rust, and grid metadata dropped before rows when the response budget is short. But it split the work across two subagents that spent its budget, stopped them mid-edit, and left Rust tests that do not compile and eight failing Python gates.
- **Codex on GLM.** Split the work across Codex subagents. Codex's own sandbox (the private home's `workspace-write` mode) refused Bubblewrap's network namespace and Chromium's sandbox host, so it could not run the browser test or the live converter. It ran out of time while fixing its own Rust test, and the embedded helper copies were left stale.

## What the console showed

I ran `by serve --app` on the scratch clone, with its own configured token, on a port of its own, from 07:50. Playwright (Chromium Headless Shell 153) took the pages at 1440 and 390 px. The screenshots beside this page:

| File | When | What it shows |
|---|---|---|
| `office-viewer-board-running-1440.png` and `-390.png` | 07:56 | Home as a board: five attempts running on one clock, `5 branches in flight · 0 need you · 0 ready · 0 done · $0.6102 so far`. Only Claude Code on Anthropic reported a running cost. At 390 px, the list only |
| `office-viewer-board-ended-1440.png` and `-390.png` | 08:51 | Every attempt ended: `0 branches in flight · 3 need you · 2 ready · 0 done · $9.54 so far`. **Merge** on the two ready rows, **Open** on the three over-budget rows |
| `office-viewer-deck-1440.png` and `-390.png` | 08:51 | Needs you as a deck: "5 decisions wait for you … One at a time, the parked agent first", the Codex attempt first as "ready to merge", and the up-next column |
| `office-viewer-slides-1440.png` | landing | The landed viewer on the three-slide PPTX: the thumbnail strip, slide 3 of 3, Fit to width, Download original, Accept |
| `office-viewer-pages-390.png` | landing | The landed viewer at 390 px on the three-page DOCX: stacked pages, `3 / 3`, the header wrapped |
| `office-viewer-print-view-1440.png` | landing | The landed viewer on the XLSX in Print View: LibreOffice's rendering of the selected sheet, page 1 of 5 |

What it got right: the board, the deck and the branch pages followed five real harnesses at once without a reload. When the turns ended, the board turned the two ready attempts green with **Merge** within a minute. The branch page of Claude Code on GLM read as a narrative ("openpyxl writes `<f>…</f><v></v>` for formula cells — so I can inject a cached value…") between its tool rows.

What it got wrong or made hard:

- **The deck's "In two hands" is a wall of text for a fan.** Every attempt of one task touches the same files by design. The first card listed every one of 20-odd files with all four other attempts ("CHANGELOG.md also in docviewer-antigravity, docviewer-claude-code, docviewer-codex-glm, docviewer-glm; …") before the question it asks.
- **Two Codex attempts cannot be told apart at 1440 px.** `docviewer-codex-glm` and `docviewer-codex-openai` both truncate to `docviewer-codex-…` in the sidebar and `docviewer-codex…` on the board. The board's status column truncates to `running · t…`.
- **The deck says nothing waits while the permission count moves.** At 07:56 and 08:10 the deck read "Nothing in branchyard waits for you." above "PERMISSION REQUESTS (2)", then (3), then (1) seconds later. The full preset answered those requests at once, but the folded count showed them.
- **"Needs you 3" beside "5 decisions".** The sidebar's badge counts the three over-budget attempts. The deck counts the two ready ones too.
- **A running branch reads "0 turns".** 20 minutes into turn 1, the Codex-on-GLM page said `codex · 0 turns`, and the Claude Code page `claude-code · 0 turns · $0.6102`.
- **Bare tool rows.** The Claude Code attempt's page after the cap was "Earlier turns are not shown here: this is the latest of 285 events" over 46 bare `Bash` rows with no narration. Codex on GLM was `fileChange` / `commandExecution` rows with occasional narration. Antigravity's page showed only the prompt for its first 20 minutes.
- **Cost.** Claude Code's figure for Claude Code on GLM appeared only when its turn ended ($7.26). While it ran, its row showed no cost.

## Friction the harnesses met

Two attempts wrote the closing list the task asked for: Codex on the ChatGPT plan and Claude Code on GLM. The cap interrupted the others. What follows comes from their messages, their logs and their candidates.

- **Codex on the ChatGPT plan.** "Friction: shared Cargo lock waits, stale embedded helpers, and nested-worktree Git detection. Added freshness coverage and documented the build requirements." It ran `tools/check.py --fast` with its scratch directory as the Git ceiling, because the attempt's worktree sits inside the clone. Its last re-check waited on the shared lock for minutes. Branchyard warned at the start that "the codex login has reached its weekly window; it resets in 2d 23h", and the turn ran to completion anyway.
- **Claude Code on GLM.** From its closing message:
  - "The two parallel subagents consumed the shared USD budget before either finished, forcing me to stop both mid-edit."
  - "`cargo clippy` took over 10 minutes behind the shared `flock`/target dir, leaving no window for `cargo test`."
  - "openpyxl read-only worksheets expose no merged cells, column widths or freeze panes, which forced hand-parsing the sheet XML."
  - "`remove_internal_paths`' field allowlist would have silently stripped the new grid keys."

  It reported "`cargo test -p branchyard-server` — never ran", which my run confirmed: the tests do not compile.
- **Codex on GLM.** `by run --harness codex` with the `zai` login is refused by the preflight unless `--no-preflight` is given (the first feature recorded the exact message: `codex is verified logged out (codex login status: Not logged in). Nothing was started.`). Inside the turn:
  - "this agent sandbox rejects Bubblewrap network-namespace setup, and direct LibreOffice exits 1 before producing PDFs".
  - "Chromium terminates with `sandbox_host_linux.cc: Check failed: . shutdown: Operation not permitted`".
  - "the outer sandbox denied its lock write" on the shared target until it re-ran the command escalated.
- **Claude Code (Anthropic).** No closing message. At the cap Branchyard warned that "the turn ended with Claude Code's background tasks still running, which ending its session stops": a real-converter browser test it had queued behind the lock.
- **Antigravity on Gemini 4 Argon.** No closing message. The stream carries only tool names (141 `read_file`, 53 `edit_file`, 44 `run_command`, 8 `manage_task`, 1 `write_file`), so the console had nothing else to show. Two more messages: "warning: step 410 reported an error", and at the cap "interrupt failed: rejected: unsupported: the Antigravity stream has no cancellation message; a signal ends the whole process".
- **Shared to all.** One Cargo lock and one target directory for eight agents on four cores. The shared target grew from 2.8 to 6.8 GB during the run, and free disk fell to 3.8 GB while the candidates' tests ran. The build rules were in the prompt only.

## Cost

| What | Billed to | Cost |
|---|---|---|
| Claude Code on Anthropic (the attempt) | the environment's Anthropic account | $2.28 |
| Claude Code as judge | the same | not recorded: `by judge --json` does not report the judge's cost, and its scratch branch is removed after the verdict |
| Claude Code on GLM (the attempt) | Z.ai's GLM plan (a subscription); the figure is Claude Code's own estimate at Anthropic prices | $7.26 as estimated |
| Codex on the ChatGPT plan | the ChatGPT plan | not reported |
| Codex on GLM | the Z.ai plan | not reported |
| Antigravity on Gemini 4 Argon | the Gemini API key, per token | not reported |

This feature's counted total is **$9.54**, with Claude Code's GLM estimate counted at face value. With the first feature's $13.44, the round's counted total is **$22.98**, under the $60 cap. The judges' spend and the Gemini and Codex spend are unknown.

Not done: a second, non-Claude judge, and a cap long enough for three of the five turns to complete, so the judges could rank every attempt. A run with the build rules in each attempt's environment and fewer attempts at once. Codex's sandbox given the shared target and Bubblewrap. The console findings above (found, not fixed). For the feature: the Docker preview backend, Safari, Firefox, a real phone or a screen reader, and Excel custom formats beyond the common number and date codes (Print View shows those). No attempt was merged through `by merge`, `by compare --pick` or `by judge --pick`: the pick was cherry-picked by hand so its commit could keep its author.
