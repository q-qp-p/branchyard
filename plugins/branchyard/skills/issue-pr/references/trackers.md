# Trackers, credentials, links, issue prompts

`--issue` works on `by run`, `by fan`, `by spawn`; `by issue REF` previews.

| Tracker | Reference | Credentials (environment) |
|---|---|---|
| GitHub | `42`, `#42`, URL | `gh auth login` |
| Linear | `linear:ENG-123`, URL | `LINEAR_API_KEY` or `LINEAR_ACCESS_TOKEN` |
| Jira | `jira:PROJ-7`, browse URL | `JIRA_EMAIL`, `JIRA_API_TOKEN`; site from URL, `JIRA_URL` or `[trackers.jira] url` |
| GitLab | `gitlab:group/project#12`, URL | `GITLAB_TOKEN`; host from URL or `GITLAB_URL` |

Keyless: the connector gateway, with `[connectors] gateway` and `[trackers.linear] gateway_tool`. `by config validate` checks them. A failed fetch quotes why (`needs LINEAR_API_KEY`) and the next step. Keys are never printed, logged or put in the prompt.

The branch is `issue-42-<slug>` (`eng-123-<slug>`); `--name` overrides. An `issue_linked` event and the prompt header record the link, so `by pr` finds it even for a server run. `by show BRANCH --json` has it in `merge_readiness.issue`.

In the PR body: GitHub `Closes #n` (`Closes owner/repo#n` across repositories), Linear `Closes ENG-123`, Jira `Refs PROJ-7 (url)`, GitLab `Related: group/project#12 (url)`. Jira and GitLab only reference. Title defaults to the issue title. `by run --issue` works with `--remote`; `by pr` is local only.

Make the issue finishable. Add as the second argument what it lacks: module to work in and not touch, the proving command as `--check` (a test that fails today), regression test and docs in the same commit, and `--budget-usd 2 --max-minutes 30 --max-turns 20`.

Shape:

- Clear bug with reproduction: `by run --issue N --check ...`.
- Open design: `--plan` proposes read-only, then `by plan approve BRANCH`.
- Unsure harness: `by fan --issue N --harness a,b` and judge (fan-judge).
- Dependent issues: one branch each, `--base` stacks them.

If the branch changed nothing or failed its check, `by log BRANCH`, `by send BRANCH "<what to try>"`; `by pr` refuses a branch that is not ready.
