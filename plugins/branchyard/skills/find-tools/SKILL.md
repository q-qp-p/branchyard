---
name: find-tools
description: "Search the tools mounted for this turn (skills, connector operations, helpers) with BM25, then show one or print an operation's schema."
---
# Find a tool

Your tools are at `$BRANCHYARD_TOOLS` (`/tools`) in a sandbox, else `.branchyard/tools/` in your home. Search before you read:

- `tools search WORDS` ranks the skills, connector operations and helpers you were given.
- `tools show ID` prints one: a skill, a connector, `CONNECTOR.OP` or a helper.
- `tools schema CONNECTOR.OP` prints an operation's input schema; else run `bin/<id> --schema`.

`tools` is `bin/tools` there. Open a skill's `SKILL.md` only after a search names it, and a connector's `reference/` only for the operation you need. Nothing else was chosen for you.
