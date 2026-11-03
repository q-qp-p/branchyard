# Console screenshots

The images in this directory show the browser console (`by serve --app --knowledge`) with demo data. A script produced every conversation, diff, cost and table. No model ran. The repository, the customers and the numbers are invented. The branches read `claude-code` because the server runs them under Claude Code's stream-json profile, with a script that speaks that protocol in Claude Code's place and reports what each turn spent; the workspace's sessions read `claude-code-acp` because they run the repository's fake agent behind a filter, under the ACP profile. Claude Code itself never ran, and every dollar figure is the script's.

| File | Shows |
|---|---|
| `console-board.png`, `console-board-dark.png` | The console's home, 1440 by 900 pixels: five tiles (in flight, need you, ready, merged today, spent), then the board, with every open branch of the Rust library on one clock that fits the few minutes the demo ran. Each turn is a bar from its start to its end with a tick for each tool call; the branch `changelog` is parked on a write the person's policy asks about, so its row is filled amber from the moment it parked, with an Answer button; interrupted, running and ready branches have Resume, Steer and Merge, and each row its cost. The three merged branches are folded under their merge ticks. Under the board, the day's decisions (one write allowed, three merges) and each branch's spend as a bar. |
| `console-decide.png`, `console-decide-dark.png` | Needs you, the deck, 1440 by 900 pixels: the parked agent's approval first, as the one raised card on the page, its header tinted by its kind, what it was doing beside what it asks, a reason field and Allow, Deny, Open and Later as its footer; the next card peeking under it, and the bay beside it with the two ready branches and the interrupted one that follow. Under it, what was decided today. |
| `console-branches.png`, `console-branches-dark.png` | A Rust library with branches in each lane: one interrupted branch and one parked on an approval under Needs you, two running, two ready, three merged under "Show 3 done". The ready branch `parser-fix` is open as a report: its prompt as the headline, the agent's reply, its five tool calls as margin notes beside the paragraphs they preceded, the turn's receipt with how long it took, its calls, commit, changes and cost, and Merge in the status strip. The Changes tab shows its diff under a file header that stays in view. 1440 by 900 pixels. |
| `console-workspace.png`, `console-workspace-dark.png` | A knowledge project's workspace with two named sessions. The open session's conversation is scrolled to its latest turns, as the console opens it, and the Outputs tab previews a CSV table the session published. 1440 by 900 pixels. |
| `console-phone.png` | The same ready branch on a phone, in one column, with Merge at the top. 390 by 844 pixels at twice the density. |

## Regenerate them

Build `by` and the fake agent, then run the generator from the repository root:

```sh
cargo build --locked --offline -p branchyard-cli --bin by -p branchyard-runtime --bin fake-acp-agent
BRANCHYARD_PREVIEW_BWRAP=/usr/bin/bwrap \
BRANCHYARD_PREVIEW_PYTHON=/srv/branchyard/artifacts/bin/python \
PLAYWRIGHT="$(npm root -g)/playwright" \
python3 tools/readme_screenshots/generate.py --by target/debug/by --agent target/debug/fake-acp-agent
```

The two preview variables name the isolated converter that renders the table ([artifact previews](../../docs/artifact-previews.md)). Without them the generator stops before it starts a server. Playwright reads `PLAYWRIGHT_BROWSERS_PATH` from the environment as usual. `--out DIR` writes the images somewhere else, and `--work DIR` puts the temporary repository and server state somewhere other than the system's temporary directory.

The generator builds nothing and calls no model. It writes the story in [`story.py`](../../tools/readme_screenshots/story.py) to a new temporary directory, starts the server, plays the story through the API and signs a desktop and a phone browser in with the server token. [`shoot.js`](../../tools/readme_screenshots/shoot.js) only opens pages and clicks. It never changes the page, and it stops before a screenshot if the page text contains the temporary directory, your home directory, the machine's name or the token.

The generator replaces the images here only after it has taken all nine. A failed run leaves them alone, keeps its temporary directory with the server's log, and prints where that directory is. `--keep` keeps it after a successful run too. The workspace images come out byte for byte the same on every run. The branch, board and deck images change between runs in commit hashes, times and the clock's window: the branches start a few seconds apart and each call is a few seconds after the last (`STAGGER` and `STEP` in `story.py`), so a run takes about three minutes and the board draws them over its last four or eight.

One branch, `changelog`, is parked on an approval: the server's configuration gives the token's principal a policy that asks before Claude Code's `Write`, the branch's task runs with the engine's policy allowing tools, and the script asks permission to write `CHANGELOG.md`. One more, `version-file`, asks the same way; the generator allows it through the API and merges it, so the day has a decision to show. No other turn asks, so the policy touches nothing else.

## Measure the home page under load

`tools/readme_screenshots/load.py` starts the same demo with busy branches added (`--busy`, `--calls`, `--step`), opens the home page for `--seconds`, and prints the requests and bytes it read, how often it rebuilt, its long tasks, and the events the branches recorded meanwhile. It then stops every branch and compares each board row with the server's timeline, and exits 1 if one differs. It needs the same variables as the generator. The numbers depend on the machine; [the console guide](../../docs/companion.md) records a run.
