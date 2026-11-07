# Fan setup: harness mix, contract, budget

Fan when approaches differ (two designs, an unknown bug cause), a second vendor is a review in itself, or `by fleet stats --kind bugfix` has no outcomes yet. Skip it for mechanical renames (one `by run` with `--check`) and for parts that own different files (`by delegate`).

Harness mix:

- `by fleet advise "<task>" --json`: candidates, availability, recorded P(OK), the router's pick. `by harnesses` lists installed ones.
- Two vendors beat three of one. `--model small` only when a check confirms the work.
- `by fan "<task>" --auto --attempts 3` routes by the fleet table and fails over; `--harness a,b` names them. `--kind` fixes the task kind that is recorded with the outcome.
- Words such as production, migration, security, payment rule out the cheapest tier.

Contract, fixed before the fan:

- Code: `--check "cargo test"`, run on every exact candidate; write it so it fails without the change. Prompt: files to touch, files to leave, commit when green. A turn with no change is ineligible.
- Answers, no Git change: `--result-schema-file S.json --result-required --result-deliverable`. Schema keywords allowed: type, properties, required, additionalProperties, items, enum, const, minimum, maximum, minLength, maxLength, pattern, minItems, maxItems, uniqueItems, anyOf, oneOf. Keep it small: `summary`, `findings` (claim, evidence), `confidence`.
- Result-only winners are read with `by compare A B --json`; `--pick` refuses them. Outside a repository, `by fan --no-files` runs a private task, read with `by task show ID --json`.

Budget: no total cap exists, each attempt has its own.

- `--budget-usd N`, `--max-turns N`, `--max-minutes M` per attempt; fan cost is about attempts times the cap. Codex, Amp and Antigravity report no cost, so only turns, minutes or `--model-gateway` bound them.
- `--stall-after M --stall-action interrupt` ends idle attempts.
- Watch with `by watch --once`, `by stats`, `by usage` (login limits a wide fan can exhaust). `by cancel BRANCH` ends a failing attempt early; keep a quarter of margin, estimates run low.
