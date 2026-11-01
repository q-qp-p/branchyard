# Dispatch: the console as a dispatcher's desk

Written 31 October 2026. The web console ([companion](../../companion.md)) stops being an inbox and becomes three surfaces for three jobs, and the engine and server gain the two abstractions those surfaces need. This page says what the console is for, why the inbox was the wrong machine, what the person's experience demands of the backend, and what was built.

## What the console is for

Branchyard runs many agents at once, each on its own branch, and asks one person to supervise them. Agent time is cheap; the person's attention is the one scarce resource in the system. The console's job is therefore to spend that attention only on decisions no agent can make, with exactly the evidence each decision needs, and otherwise to show that the yard is healthy without being opened.

## Why the inbox was wrong

The desktop console ([design](2026-10-29-desktop-console-design.md)) grouped branches into lanes and opened each in a conversation pane, like a mail client. An inbox's unit is a message, and every row has the same shape. The console's unit is a decision, and decisions have different shapes:

| Decision | Who waits | Clock | Evidence it needs | Answer |
|---|---|---|---|---|
| Approval | the agent, parked mid-turn | none: the turn waits at the approval until someone answers | the call and its arguments, which policy asked, what the branch was doing | allow, deny |
| Question or escalation | the agent, parked | none | the question, what it was doing | a sentence; approve, deny |
| Merge | delivery; no agent | none, but main drifts | what it says, what it ran, the files, the diff, who else holds those files | merge (held), follow up |
| Stop | nobody | none | the failure, the last thing it did, cost so far | resume, follow up |
| Stall | nobody | none | how long it has been silent | look, cancel |

An inbox sorts these by arrival, puts a parked agent under a merge review, forces open-read-find-act-return on every one, has no time axis, no liveness and no lineage, and makes the sidebar fight the detail for the same width. Its action button sits top right, as far from the diff as the page allows.

## How the experience shapes the abstractions

Each surface asked the backend for one thing it could not say.

**A branch must say it is parked.** The engine recorded an approval ask in the effects store and waited for it inside the turn; the branch's status stayed `running`, and only a separate approvals list knew. Every surface that shows a branch (the lanes, the board, `by ls`, the stall detector) had to join that list itself, or lie. So `BranchInfo` gains `awaiting`: what the running turn is parked on, with the ask's id, what it asks and since when. It is read from the pending asks when a record is listed or looked up, never stored, so it is exact the moment the ask is answered, and it is `None` whenever the branch is not running. This was a field and not a new `BranchStatus` variant on purpose: `running` is the lifecycle state (a harness process is alive, a cancel still applies, the lease holds), and "parked" is a property of that turn, like `stalled`. A parked turn is never stalled; it moves only when someone answers.

**A decision must be a resource.** The deck wants the decisions in the order to take them, and the order is a rule the client should not have to know: a parked agent first, the one that has waited longest before the rest, because it moves only when answered; then the branches ready to merge, because delivery waits on them; then the stopped and stalled, which nothing waits on. Nothing expires. So the server gains `GET /v1/repos/{repo}/decisions` (`DecisionList`): each decision carries its kind, its branch, who waits (`agent`, `delivery`, `nobody`), since when where the server knows, the branch's status and candidate, and the ask or the question itself. The console had been assembling questions from every delegating branch's inbox on its own; that moved into the server with the rest.

**The turn is a record the client can read.** The report view needs each tool call beside the paragraph it produced, and a receipt per turn. The event page already carries both in order, so the client's conversation reducer attaches the calls that precede a paragraph to that paragraph and keeps each turn's start and end. No server change. Per-turn cost would need the engine to report cost at each turn's end; the receipt shows time, calls and the diff, and the strip keeps the branch's total.

Two things stayed deliberately client-side or were left for later: hold-to-merge and the undo window are mechanisms of the page (an allow or a deny is held for eight seconds before it is sent, and the agent stays parked meanwhile; a merge is a 700 ms hold, because the server offers no undo for it), and the ground plan (which files are in two hands) came in a follow-up: `GET …/ground` reads every open branch's diff when asked and lists each changed path with the open branches that changed it, the branch page's inspector gains a Ground tab beside Changes, and a merge card names the files another open branch also holds.

## The surfaces

**Decide** (`#/attention`, which `#/inbox` and `#/approvals` still open): one card at a time, shaped for its decision, the question at the top, the evidence in the middle, the answers at the bottom. Keys: Enter takes the first action (held, for a merge), `n` denies, `s` sends the card to the back, `o` opens the branch, `f` opens it with the message box focused. The bay beside the card shows the answers held for undo, then what is next, parked agents first. A merge card shows what the agent said last, what its calls did (the harness reports what ran, not whether it passed), the changed files with their counts, and the first lines of the first file's diff.

**Watch** (`#/`, home): every open branch on one clock, the last hour to scale, now at the right edge. A running branch's bar is hatched; a wait on the person fills amber from the moment the turn parked; a ready branch draws a thin green line until it is merged; merged branches fold to ticks. Each row has its cost and one action: Answer, Merge, Steer, Resume or Open. Steer opens a one-line box under the row, so a sentence reaches a running turn without opening the branch.

**Work** (`#/b/…`): the turn as a report. The agent's text is a column with a margin; each tool call is a note in the margin beside the paragraph it preceded, named after the tool, and opens the Activity tab. Each turn ends with a receipt: how it ended, how long it took, how many calls read, edited and ran, and the commit with its counts. On a phone the margin becomes a line above the paragraph. The "N tool calls" fold is gone.

The sidebar stays. It is the navigation, its lanes already read `awaiting`, and removing it was not what made the inbox wrong. The strip keeps its confirmed Merge; the deck's merge is the held one.

## Evidence

Hermetic. `crates/branchyard/tests/effects.rs` shows a branch parked on a tool approval listed and looked up with `awaiting` naming the ask, and none once answered. `crates/branchyard-server/tests/effects.rs` shows the server's decisions putting the parked branch first, waiting on the agent, with the ask attached, and the decision gone once answered. `tests/companion_browser.rs` runs a server whose person policy asks before a write: the board draws the parked branch with its amber wait, the lane says "waiting for you", the tab title counts it, the deck shows the ask first, Later sends it to the back and it comes round, Enter holds an allow that Undo takes back with nothing sent, Enter again sends it after the window, the branch finishes and comes back as a merge card with its changed file. The same run shows a tool call as a margin note that opens Activity. Not done: a live model, a real phone, Safari, Firefox, a screen reader, a question from a delegating branch in the browser (the server test covers the ordering, not the page), per-turn cost, and the ground plan (added since; see [the companion's status](../../companion.md)).
