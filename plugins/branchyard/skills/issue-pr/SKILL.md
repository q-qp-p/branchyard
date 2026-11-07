---
name: issue-pr
description: "Turn tracker issues (GitHub, Linear, Jira, GitLab) into branches and linked pull requests."
keywords: [issue, ticket, linear, jira, gitlab]
---
# Ticket, branch, PR

For a session that runs Branchyard as the person or meta-harness, not a spawned leaf. The tracker's issue is the task; the pull request closes or links it.

1. **Preview.** `by issue 42` (or `linear:ENG-123`, `jira:PROJ-7`, `gitlab:group/project#12`, or a URL) prints the title, the branch name and the prompt a run would send, and starts nothing. Add instructions as a second argument.
2. **Start.** `by run --issue 42 --check "cargo test" --yes` fetches the issue (GitHub through `gh`; other trackers with `LINEAR_API_KEY`, `JIRA_EMAIL` and `JIRA_API_TOKEN`, `GITLAB_TOKEN`) and starts branch `issue-42-<slug>`. Without a good check, the branch is not done when it says so. Not sure of the harness or approach: `by fan --issue 42 ...` and judge (fan-judge).
3. **Read the candidate.** `by show BRANCH`, `by diff BRANCH`, `by check BRANCH`; send fixes with `by send BRANCH "..."`.
4. **Open it.** `by pr BRANCH` links the issue in the body: `Closes #42`, `Closes ENG-123` for Linear, `Refs PROJ-7 (url)` for Jira, `Related: group/project#12 (url)` for GitLab.
5. **Follow it.** `by pr BRANCH --watch --yes --max-rounds 3` (pr-feedback-loop), then merge with `gh pr merge`.

Credentials stay in the environment and are never printed or put in the prompt.

References: `references/trackers.md` (references, credentials, links, making an issue finishable).
