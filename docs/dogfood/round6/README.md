# Dogfood round 6

Written 1 November 2026. Branchyard building a Branchyard feature with real harnesses: the same task, given verbatim to Claude Code (Anthropic), Claude Code on Z.ai's GLM, Codex (OpenAI and GLM) and Antigravity (Gemini 4 Argon) through `by fan` and `by run`, watched in the console, judged by the deterministic judge and by Claude Code, compared, and the best attempt landed on the branch `html-artifacts`. The same day the four drivers were qualified live ([driver qualification](../../qualification/README.md#results-1-november-2026)). Feature 2 of the round, Office documents in a real viewer, is recorded by its own agent in `feature2-office-viewer.md` beside this page.

Files beside this page: `qualification/` (the second reports of profiles whose committed report is another run), and the console screenshots `console-*.png` (1440 and 390 px wide).

## What ran

Everything ran in one Linux container (kernel 6.18.44, four cores) on 1 November 2026, between 06:30 and 09:15 UTC, with `by` 0.3.1 built from this branch's base (`7746ef1e`, `dispatch` with `main` merged) and copied out of the shared target so other builds could not replace it.

| Harness | Version | Model | Credentials |
|---|---|---|---|
| Claude Code | 2.1.316 | `claude-sonnet-5-5` (its default; named in every reply of the session transcripts) | the environment's sign-in, which reaches a private home through the proxy; nothing copied |
| Claude Code on Z.ai | 2.1.316 | `glm-4.6`, then `glm-5.3` (`--model`) | `by harnesses login claude-code --provider zai` in a private user configuration whose `[secrets]` names the variable holding the key |
| Codex | codex-cli 0.166.0 | `gpt-6.1-sol` under a ChatGPT plan login; the OpenAI API key in the environment had no credit | the plan login in a private home (local mode, the harness's `HOME` set to it) |
| Codex on Z.ai | codex-cli 0.166.0 | `glm-4.6`, then `glm-5.3` | `by harnesses login codex --provider zai` as above (Branchyard writes `model_provider = "zai"` into the private home's `config.toml`) |
| Antigravity | 1.4.12 (fetched pinned at 1.4.9 with `tools/fetch_agy.py`; it replaced itself on its first start) | `gemini-4-argon-high` | `GEMINI_API_KEY` as a `--secret` |

The round's third harness was first Gemini CLI 0.66.0; the person changed it to Antigravity on Gemini 4 Argon before any Part B attempt started, so Gemini CLI appears only in the qualification.

**The task.** "Build this Branchyard feature in this repository: a vibe-coded HTML artifact can be seen and used from the console", with the five acceptance points below given verbatim as the check, the repository's build rules for parallel worktrees (one shared, pre-built target directory under one lock, `CARGO_HOME` for the offline registry because the harness's home is private), and a request to list what could not be done and the friction met.

1. An HTML output previews as a page image like a DOCX does: rendered by headless Chromium (the pre-installed one; never `playwright install`) inside the same Bubblewrap sandbox the converter uses (no network, private home, read-only source), through the existing preview service, with `GET …/preview` answering `passed` and `pages`.
2. "Open" shows the HTML running in a sandboxed iframe: served from a dedicated path with `Content-Security-Policy: sandbox allow-scripts; default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src data:` (or stricter), framed only by the console (`frame-ancestors 'self'` on that path, while the rest of the console keeps `X-Frame-Options: DENY`), no access to the console's origin storage or token (the iframe gets `sandbox="allow-scripts"` without `allow-same-origin`), and the viewer sees a counter button work.
3. Hermetic tests: the preview path with a fake renderer in `tests/test_artifact_previews.py` and the Rust preview tests; a browser step in `crates/branchyard-server/tests/console_workspace_browser.{rs,js}` where an HTML output with inline CSS and JS previews as a page and opens in the iframe, with the counter clicked inside it; `every_asset_is_self_contained` and the CSP tests still green.
4. Docs: `docs/artifact-previews.md` and `docs/companion.md` (a dated status paragraph with evidence and "Not done:"), CHANGELOG.md Unreleased.
5. The repository's gates: `python3 tools/check.py --fast`, `cargo clippy -p branchyard-server --all-targets -- -D warnings`, `cargo test -p branchyard-server`.

**The runs.** From a scratch clone: `by fan "<task>" --harness claude-code,codex --isolated --permissions full --check "python3 tools/check.py --fast" --budget-usd 8 --max-minutes 45 --secret CODEX_API_KEY --name html-artifact --json`, and with the same caps one `by run` each for Antigravity (`--harness antigravity --model gemini-4-argon-high --allow-unapproved-tools --secret GEMINI_API_KEY`), Claude Code on GLM and Codex on GLM (`--model glm-4.6`, then `glm-5.3`, from a user configuration holding only the Z.ai logins), and Codex on the ChatGPT login (`--harness codex` without `--isolated`, its `HOME` the signed-in home). Two waves: the fan, Antigravity and the two `glm-4.6` attempts started between 06:40 and 06:44 and all ended by 07:27; Codex on the ChatGPT login (07:43) and the two `glm-5.3` attempts (07:55) ran after the person's corrections, beside a second dogfood agent's five attempts on another feature. So four real attempts ran at once in the first wave and eight in the second, on four cores with every Cargo build under one lock; the load average was 8.8 at 08:13 and 16 at 09:02. That is most of why six of the eight attempts reached the 45-minute cap. Every attempt had the same task text; the build rules were in the prompt only, not in the attempts' environment.

## Result

Costs are what `by ls --json` records (`cost_usd`); `-` is a harness that reports none. "Check" is the attempt's `--check` as the judges ran it on its exact candidate; "Gates" is what I ran on each candidate under my own lock (`python3 tools/check.py --fast`, `cargo fmt --check`, `cargo clippy -p branchyard-server --all-targets -- -D warnings`, `cargo test -p branchyard-server` with the browser tests and a real Bubblewrap and Chromium). The deterministic score is 0 for an attempt whose last turn did not complete; the Claude Code judge's scores are from its verdict over all eight (the first-wave verdict, over five, is in brackets).

| Attempt | Harness | Model | Outcome | Turns | Wall time | Cost | Check | Gates | Deterministic | Claude Code judge |
|---|---|---|---|---|---|---|---|---|---|---|
| `html-artifact-claude-code` | Claude Code 2.1.316 | `claude-sonnet-5-5` | over budget (max_duration), 26 files +854 −42 | 1 | 45 min 2 s | $2.50 | passed | all pass: 443 tests in 35 binaries, 0 failed; the browser test printed `HTML output: pages` | 0 (not eligible) | **72**, ranked first (72) |
| `html-artifact-codex-openai` | Codex 0.166.0 | `gpt-6.1-sol` (ChatGPT plan) | **ready**, 27 files +585 −29 | 1 | 35 min 39 s | - | passed | all pass: 443 tests, 0 failed | **95**, the proposed pick | 60, ranked second |
| `html-artifact-codex` | Codex 0.166.0 | default (OpenAI API key) | failed: `Quota exceeded. Check your plan and billing details.` | 1 | 1.4 s | - | no candidate | - | 0 | 0 (0) |
| `html-artifact-glm` | Claude Code 2.1.316 | `glm-4.6` | over budget, 17 files +872 −30 | 1 | 45 min | $5.38 (Claude Code's price; Z.ai bills the plan) | failed (`ruff_format`) | tests do not compile | 0 | 45 (50) |
| `html-artifact-glm53` | Claude Code 2.1.316 | `glm-5.3` | over budget, 15 files +881 −21 | 1 | 45 min | $5.18 (as above) | failed | `ruff_check`, `ruff_format`, `artifact_assets`, `docs_facts` fail; the library does not compile | 0 | 42 |
| `html-artifact-codex-glm` | Codex 0.166.0 | `glm-4.6` | over budget, 17 files +1042 −34 | 1 | 45 min | - (22.4M tokens, mostly cached) | failed (`check_handrolled`) | tests do not compile; clippy `too_many_arguments` | 0 | 28 (42) |
| `html-artifact-codex-glm53` | Codex 0.166.0 | `glm-5.3` | over budget, 23 files +888 −29 | 1 | 45 min | - (24.9M tokens) | failed (`docs_facts`) | clippy clean; 439 passed, 4 failed (the browser test and its own HTML route test among them) | 0 | 38 |
| `html-artifact-antigravity` | Antigravity 1.4.12 | `gemini-4-argon-high` | over budget, 16 files +573 −27 | 1 | 45 min | - | failed (`docs_facts`) | the library does not compile (`Result::is_err_or`) | 0 | 30 (35) |


**The pick: the Claude Code attempt**, cherry-picked onto `html-artifacts` with its author kept (`10bac6ed`, then my comment fix `9fe0288d`). It is one of the two attempts whose check passed, it meets all five acceptance points, and the reasons for it over the other are below; the only change made to it is a comment fix in the browser test. On that branch: `python3 tools/check.py --fast` passed; `cargo fmt --check`, `cargo clippy -p branchyard-server --all-targets -- -D warnings` and `cargo doc -p branchyard-server` with `RUSTDOCFLAGS="-D warnings"` were clean; `cargo test -p branchyard-server` passed 443 tests in 35 binaries (0 failed, the three browser tests included) with no preview renderer configured, and the workspace browser test passed again with Bubblewrap and the preview Python set, printing `HTML output: pages`: a real headless Chromium inside the namespace rendered the page, and the iframe step clicked the counter to 2, saw the page's `fetch` refused and `sessionStorage` throw `SecurityError`, read the frame path's policy exactly and found `X-Frame-Options: DENY` still on `/app/`.

**The judges and the pick disagree, for a reason in the judges.** A judge scores only an attempt whose last turn completed, and the 45-minute cap ends a turn as `over budget (max_duration)`, so seven of the eight attempts scored 0 deterministically whatever their tests said (`not pickable: its last turn did not complete with changes`). In the first wave no attempt was eligible and neither judge proposed a pick; the deterministic ranking of all-zero scores fell back to branch names, so its first row was `html-artifact-antigravity`, which read like a verdict and was not one. Over all eight, both judges proposed the Codex attempt, the only one that completed; the Claude Code judge nonetheless ranked the Claude Code attempt above it (72 to 60: "loads the HTML into the iframe through postMessage and document.write rather than from a dedicated sandboxed path as the task requires, and its CHANGELOG entry is thin"). A Claude Code judge ranking a Claude Code attempt first is worth a second judge; none was run.

I read both passing diffs and ran both under my lock. Both meet the five points in substance and pass every gate. The Claude Code attempt is the pick because it binds Chromium into the namespace for HTML sources only (Codex's binds `/opt/pw-browsers` into every conversion, Office included), lets an operator name the browser (`BRANCHYARD_PREVIEW_CHROMIUM`; Codex's searches `/opt/pw-browsers` and `PATH` only), serves the page from its own path as point 2 asks, and its browser step checks more of point 2 (the page's `fetch` refused, `sessionStorage` throwing `SecurityError`, the exact policy, `X-Frame-Options: DENY` kept on `/app/`). What Codex's does better: nothing that can read the page's bytes is reachable without the bearer token (the viewer path is a static shell that receives the bytes by `postMessage`, where Claude Code's ten-minute ticket address serves them to whoever holds it while the minting caller's rights hold), and its browser step requires the HTML preview to answer `pages` even without a renderer, through a fake one; but that fake replaces the real renderer for every format in that test, so DOCX and PPTX no longer render live there, which is how the 1 November LibreOffice fix was seen working. Merging Codex's viewer into the pick would have been a redesign, not a cleaner result, so nothing was merged.

What each one got right and wrong, from the diffs:

- **Claude Code (Anthropic)**: everything in the acceptance list, docs with a dated status paragraph and "Not done:", the changelog, the SDK route-parity list (both new routes excused as console-only); removed the 2 GiB address-space limit for HTML sources because Chromium crashed under it (exit -5), and said so in the docs.
- **Codex (ChatGPT plan)**: everything in the list, a dated evidence record (`docs/evidence/console-html-bubblewrap-2026-11-01.json`), an extra `GET …/sources/{source}/open`; the trade-offs above; a two-line changelog entry.
- **Claude Code on GLM**: a reasonable single-use ticket design on `/v1/…/open`, let through the auth middleware by a path-and-query match; no docs, no changelog, no browser step, tests that do not compile (`glm-4.6`); on `glm-5.3` a "view handle" design, but the library does not compile and four Python gates fail.
- **Codex on GLM**: `glm-4.6` built an elaborate ticket store (hashes, scopes, an eviction order) with a hand-rolled shell quoter in its test; `glm-5.3` compiled, kept clippy clean and failed 4 of 443 tests and the docs-facts gate (two undocumented routes): the closest of the GLM attempts.
- **Antigravity**: an `/open` route for three kinds of original and a preview path, no browser step, no docs, a library that does not compile.

## What the console showed

`by serve --app` on the scratch clone, with a configured token and no harness commands, watched the yard from 06:41; Playwright (Chromium Headless Shell 153) took the pages at 1440 and 390 px. The screenshots beside this page:

| File | When | What it shows |
|---|---|---|
| `console-board-running-1440.png` | 06:56 | Home as a board: four attempts running on one clock, the quota-failed Codex attempt amber, `4 branches in flight · 1 needs you · $1.74 so far`; only Claude Code reports a cost |
| `console-board-running-390.png` | 06:56 | The same at phone width: the list only, no horizontal scroll |
| `console-branch-claude-code-running-1440.png` | 06:56 | A running Claude Code attempt: Bash calls in the margin beside its narration ("The auth middleware blocks `/app/frame/…`; checking its public-path rules.") |
| `console-branch-codex-glm-running-1440.png` | 06:56 | Codex on GLM: `commandExecution` rows and its progress notes |
| `console-branch-antigravity-running-1440.png` | 06:56 | Antigravity: a column of bare `read_file` and `run_command` rows; agy's stream carries no narration and no arguments |
| `console-board-ended-1440.png` | 07:28 | Every attempt over its time cap: five amber rows, `0 branches in flight · 5 need you · $7.88 so far` |
| `console-deck-ended-1440.png` and `-390.png` | 07:28 | Needs you as a deck: the Claude Code attempt first, "the last thing it said", its last calls, **Follow up** as the first action |
| `console-branch-claude-code-ended-1440.png` and `-390.png` | 07:28 | The picked attempt's page: 1 turn, $2.50, `Over budget (max_duration)`, 26 changed files with their bars, the turn's receipt |
| `console-board-final-1440.png` | 08:45 | After the second wave: eight attempts, `7 need you · 1 ready · $13.06 so far`, the ready Codex attempt the one green row, with **Merge** |
| `console-branch-codex-openai-ready-1440.png` | 08:45 | The Codex attempt's page: its closing message as a report (Passed, Not done, Friction), **Merge**, 27 changed files |

What it got right: the board, the deck and the branch pages followed five real harnesses at once without a reload, the deck put the quota failure in front of a person within a minute, and the branch page's margin notes made a Claude Code or Codex attempt readable while it ran. What it got wrong or made hard:

- **`last active [object HTMLTimeElement]`** on every deck card (`app.js`, the deck line builds its text with `ago(...)`, which returns an element). A bug; not fixed here.
- **Five attempts of one task cannot be told apart in the sidebar at 1440 px**: every name truncates to `html-artifact…`. The board shows a few more characters, not enough.
- **The age beside a running branch is not "last activity"**: at 06:56 two attempts read `running · 1s` and `3s`, and two that were also calling tools read `running · 15m`.
- **A billing failure is offered "Follow up"** as its first action, which cannot fix it.
- **Turn receipts get the duration wrong**: the picked attempt's reads `33m 52s` for a turn that ran 45 minutes (06:40:48 to 07:26:04), and the Codex attempt's reads `Ready · 1m 31s` for a turn of 35 minutes.
- The Changes list renders `docs/evidence/console-html-bubblewrap-2026-11-01.json` as `dconsole-html-bubblewrap-2026-11-01.json`: the truncation keeps the directory's first letter.
- `by ls` showed the Claude Code attempt at $2.72 at 07:17 and $2.50 at the end; its final `turn cost` is $2.50, so a running cost can go down.
- `by judge --deterministic` prints `local mode: harnesses run as your user, in your own HOME` and `tool permission requests will be denied` though it runs no harness.

## Friction the harnesses met

Only the Codex attempt on the ChatGPT plan wrote the closing list the task asked for; the time cap interrupted the others before their last message. What follows is from their logs, their candidates and the runs around them.

- **Claude Code (Anthropic).** Wrapped the task's `flock … cargo` template in scripts of its own under `$TMPDIR` and drove every build through them; found by itself that the auth middleware refused `/app/frame/…` and added the path to the public set; found that Chromium crashes (exit -5) under the preview helper's 2 GiB address-space limit and lifted that limit for HTML sources only; spent its last minutes on formatting and on the SDK route-parity list for its two new routes when the cap stopped it. Its candidate passed every gate as it stood.
- **Codex on the ChatGPT plan.** Its closing message, the only one any attempt wrote: "Friction: Cargo contention, cache permissions, global Git signing, `TMPDIR` Git discovery, and Chromium startup limits." Full Chrome stalled on desktop-service start inside the namespace, so it prefers Chrome Headless Shell; the machine's global Git signing (`commit.gpgsign` with a signing program) broke a fixture until it ran the gates with neutral Git settings; its final re-check waited more than twelve minutes on the shared Cargo lock, and it cancelled it. Branchyard's preflight warned at the start that "the codex login has reached its weekly window; it resets in 2d 23h"; the turn ran to completion anyway.
- **Codex on GLM (4.6 and 5.3).** `Model metadata for glm-4.6 not found` (and for `glm-5.3`) at every turn start, and twelve `Reconnecting...` warnings in the 5.3 run. Codex's own `workspace-write` sandbox made the shared target directory read-only, so the 4.6 attempt built into `/tmp/html-target-fan` (5.5 GB, on a disk that fell to 620 MB) until it was steered back with `by send --steer`; then `sccache` failed inside the sandbox. `by run --harness codex` with the `zai` login was refused by the preflight before any branch existed: `codex is verified logged out (codex login status: Not logged in). Nothing was started.` (`--no-preflight` ran it). The 4.6 candidate hand-rolled a shell quoter in a test (`check_handrolled`) and its tests did not compile; the 5.3 candidate passed clippy and failed only `docs_facts` (two new routes undocumented).
- **Claude Code on GLM (4.6 and 5.3).** One `API retry 1 of 10`. Neither candidate compiled its tests (a Python `{log!r}` inside a Rust `format!` string, mismatched types), and neither wrote docs or a changelog entry; Claude Code priced the 4.6 turn at $5.38 and the 5.3 turn at $5.18, about twice the Anthropic attempt, because it prices GLM's tokens as its own models'.
- **Antigravity on Gemini 4 Argon.** The stream carries tool names only (`read_file`, `run_command`, `manage_task`), no narration and no arguments, so the console had little to show; one `step 85 reported an error`; at the cap `interrupt failed: rejected: unsupported: the Antigravity stream has no cancellation message; a signal ends the whole process`. The candidate did not compile (`Result::is_err_or` does not exist), and its three new `/open` routes were undocumented (`docs_facts`). The binary updated itself from the pinned 1.4.9 to 1.4.12 on its first start.
- **Codex on the OpenAI API key.** `Quota exceeded. Check your plan and billing details.` 1.4 seconds after the turn started: the key had no credit.
- **Shared to all.** Every attempt queued on one Cargo lock with up to eight real agents on four cores (the load average was 8.8 at 08:13 and 16 at 09:02; the first wave's was not measured), and the build rules were only in the prompt. Nothing put `CARGO_BUILD_JOBS` or the target directory in the attempts' environment.

## Cost

| What | Billed to | Cost |
|---|---|---|
| Claude Code on Anthropic: a sign-in probe ($0.016), its qualification ($0.0637), its attempt ($2.50) | the environment's Anthropic account | $2.58 |
| Claude Code as judge, twice (five and eight attempts) | the same | not recorded: the judge's scratch branch and its home are removed after the verdict, and neither `by judge --json` nor the yard keeps its cost |
| Claude Code on GLM: qualifications on `glm-4.6` ($0.23) and `glm-5.3` ($0.0704), attempts on `glm-4.6` ($5.38) and `glm-5.3` ($5.18) | Z.ai's GLM Coding Plan (a subscription); the figures are Claude Code's own estimate at Anthropic prices | $10.86 as estimated |
| Codex on the ChatGPT plan (qualification and attempt) | the ChatGPT plan; the plan's weekly window had been reached before the attempt started (`by`'s usage warning), and the turn ran anyway | not reported |
| Codex and Claude Code on GLM, Codex's qualifications on GLM | the Z.ai plan | not reported |
| Antigravity on Gemini 4 Argon (qualification and a 45-minute attempt) and Gemini CLI (qualification) | the Gemini API key, per token | not reported by either harness |
| Codex on the OpenAI API key | nothing: no call was served | $0 |

The round's running total, as kept in the status file, counts the first two rows and Claude Code's estimates for the GLM runs at face value: **$13.44**, under the $60 cap, with the judges' and Gemini's spend unknown.

## Qualification (Part A)

The four drivers, qualified live the same morning with `crates/branchyard-qualify` ([full table and findings](../../qualification/README.md#results-1-november-2026)):

| Profile | Harness and model | Result | Time | Cost |
|---|---|---|---|---|
| `claude-code-stream-json` | Claude Code 2.1.316, `claude-sonnet-5-5` | 9 of 9 | 24 s | $0.0637 |
| `claude-code-stream-json` | Claude Code 2.1.316, `glm-5.3` through Z.ai (first run on `glm-4.6`: 9 of 9 in 70 s) | 9 of 9 | 62 s | $0.0704 (Claude Code's estimate) |
| `codex-app-server` | codex-cli 0.166.0, `gpt-6.1-sol` on a ChatGPT plan login | 9 of 9 | 38 s | not reported |
| `codex-app-server` | codex-cli 0.166.0, `glm-5.3` through Z.ai (first run on `glm-4.6`: 9 of 9 in 49 s) | 9 of 9 | 46 s | not reported |
| `codex-app-server` | codex-cli 0.166.0, the OpenAI API key | 1 of 9: the key has no credit (`429 insufficient_quota`) | 729 s | none |
| `antigravity-stream-json` | Antigravity 1.4.12 (pinned 1.4.9, self-updated), `gemini-4-argon-high` | 9 of 9 (four declared) | 26 s | not reported |
| `gemini-cli-acp` | Gemini CLI 0.66.0, `gemini-3.8-flash` | 9 of 9 | 126 s | not reported |

Not done: a second, non-Claude judge (the Claude Code judge ranked a Claude Code attempt first); a run with the build rules in each attempt's environment (`CARGO_BUILD_JOBS`, the shared target) rather than its prompt, and with fewer attempts at once; a cap long enough for the slower attempts to finish, so the judges could score them; Codex's sandbox given the shared target as a writable root; the deck's `[object HTMLTimeElement]`, the receipt durations and the sidebar truncation (found, not fixed); the Docker preview backend, Safari, Firefox and a real phone for the feature; the cost of the two judge runs and of the Gemini and Codex runs, which no harness reported. No attempt was merged through `by merge`, `by compare --pick` or `by judge --pick`: the pick was cherry-picked by hand so its commit could keep its author.
