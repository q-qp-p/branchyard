# Reading cost and telemetry

- `by watch` (`--once` prints one frame): status and cost of every branch, live.
- `by show BRANCH`: cost, budget left, tree budget under a limit. `--json`: `cost_usd`, `max_usd`, `remaining_usd`, `subtree_cost_usd`, `tree_remaining_usd`; `max_usd` is null without a limit.
- `by log BRANCH`: a `turn cost $0.03` line per turn. None when the harness reports nothing priceable.
- `by stats`: branches, turns, cost totals. `by ls --json`: all branches.
- `by fleet stats --kind research`: runs, success, mean cost, time and turns per candidate; the base rate for the next budget. `by fleet advise "<task>"` adds what can run here.
- `by usage`: each Claude Code and Codex login's 5-hour and weekly usage and reset time; check before a wide exploration.
- `by compare`: cost and time per attempt side by side.
- `--telemetry URL` sends the harness's OpenTelemetry to an OTLP/gRPC collector; `--telemetry off` disables it. Limits compare against Branchyard's own figures above, not the collector's.

Reading honestly: cost is the harness's estimate or the catalog's price; an unpriced model counts as free; streaming output makes it run low. A missing cost line means unknown, not zero: use turns, minutes or the gateway. Judge cost per finding, not per turn, and keep the number with the answer so the next run starts from it.
