# Issue tracker: GitHub

Issues and PRDs for this repo live as GitHub issues. Use the `gh` CLI for all operations.

## Conventions

- **Create an issue**: `gh issue create --title "..." --body "..."`. Use a heredoc for multi-line bodies.
- **Read an issue**: `gh issue view <number> --comments`, filtering comments by `jq` and also fetching labels.
- **List issues**: `gh issue list --state open --json number,title,body,labels,comments --jq '[.[] | {number, title, body, labels: [.labels[].name], comments: [.comments[].body]}]'` with appropriate `--label` and `--state` filters.
- **Comment on an issue**: `gh issue comment <number> --body "..."`
- **Apply / remove labels**: `gh issue edit <number> --add-label "..."` / `--remove-label "..."`
- **Close**: `gh issue close <number> --comment "..."`

Infer the repo from `git remote -v` — `gh` does this automatically when run inside a clone.

## Pull requests as a triage surface

**PRs as a request surface: no.** _(Set to `yes` if this repo treats external PRs as feature requests; `/triage` reads this flag.)_

When set to `yes`, PRs run through the same labels and states as issues, using the `gh pr` equivalents:

- **Read a PR**: `gh pr view <number> --comments` and `gh pr diff <number>` for the diff.
- **List external PRs for triage**: `gh pr list --state open --json number,title,body,labels,author,authorAssociation,comments` then keep only `authorAssociation` of `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, or `NONE` (drop `OWNER`/`MEMBER`/`COLLABORATOR`).
- **Comment / label / close**: `gh pr comment`, `gh pr edit --add-label`/`--remove-label`, `gh pr close`.

GitHub shares one number space across issues and PRs, so a bare `#42` may be either — resolve with `gh pr view 42` and fall back to `gh issue view 42`.

## When a skill says "publish to the issue tracker"

Create a GitHub issue.

## When a skill says "fetch the relevant ticket"

Run `gh issue view <number> --comments`.

## Native relationships

GitHub represents blocking edges and parent/child natively, and the native form is the one the GitHub UI and `/wayfinder` read. The prose sections stay, and are not decoration: a tool that reads the ticket graph may parse either, and some read a ticket's parent from prose alone. Write both.

The two integers in play are not interchangeable, and both are plausible in either position. **The URL path always addresses an issue by its `#number`** — the same number you would type in the UI. **The relationship being added or removed is always identified by the other issue's numeric `database id`**: the `issue_id` / `sub_issue_id` body field, and the id at the end of the dependency DELETE path. Read a database id with `gh api repos/{owner}/{repo}/issues/<n> --jq .id`, never the `node_id`, and send it with `gh api`'s `-F` (typed integer), never `-f` — `-f` sends a string, and the API rejects it with a 422 naming the wrong type. A database id in the path 404s; an issue number in the body wires the wrong issue or fails.

- **Add a blocking edge**: POST on the **blocked** issue, carrying the **blocker's** id — `gh api --method POST repos/{owner}/{repo}/issues/<blocked>/dependencies/blocked_by -F issue_id=<blocker-db-id>`. Remove one with `gh api --method DELETE repos/{owner}/{repo}/issues/<blocked>/dependencies/blocked_by/<blocker-db-id>`.
- **Link a sub-issue**: POST on the **parent**, carrying the **child's** id, on the **plural** path — `gh api --method POST repos/{owner}/{repo}/issues/<parent>/sub_issues -F sub_issue_id=<child-db-id>`. Removal uses the **singular** path — `gh api --method DELETE repos/{owner}/{repo}/issues/<parent>/sub_issue -F sub_issue_id=<child-db-id>`. The two paths differ by that `s`.
- **Read the current state before writing**, and **paginate** — without `--paginate` these return only the first page, so an edge past it is invisible: re-added into a 422, or left stale and never removed. `gh api --paginate repos/{owner}/{repo}/issues/<n>/dependencies/blocked_by --jq '.[].number'` and `gh api --paginate repos/{owner}/{repo}/issues/<n>/sub_issues --jq '.[].number'` — one number per line, since `--jq` runs per page and an array-wrapping filter would emit one array per page. A child's parent is a single object and needs no paging: `gh issue view <n> --json parent --jq '.parent.number'`.
- **Re-adding an existing relationship is a 422, not a no-op** (`Target issue has already been taken`; `Issue may not contain duplicate sub-issues and Sub issue may only have one parent`). Read-then-post is therefore what makes a re-run idempotent — the error is not a safe substitute, because it aborts the pass on the first edge that already exists.
- **A sub-issue has exactly one parent.** The parent's `sub_issues` list is silent about a child parented somewhere else, which is what the `--json parent` read is for. Never silently detach a child from an existing parent: re-parent only when the set being published is the one that declares it, and otherwise leave it and say so.
- **A re-run over an edited set leaves stale edges.** A native dependency the ticket no longer declares still gates it, and unlike a prose line it leaves no visible residue in the body. Delete the edges the current declaration dropped — including a parent link the ticket has moved away from, which must be removed before the new one can be added.
- **A 410 means the feature is gone. A 404 means almost anything** — an unavailable endpoint, but equally a wrong repo, a mistyped number, an issue you cannot see, or a database id where the path wanted a `#number`. Do not read a bare 404 as feature unavailability: confirm the issue itself resolves (`gh issue view <n>`) and only then, if the relationship endpoint still 404s, call the feature unavailable, degrade to prose-only, and say so once. A 404 that survives no such explanation is a real failure — report it rather than swallowing it, because an edge that silently did not land makes a blocked ticket look ready.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: a single issue labelled `wayfinder:map`, holding the Notes / Decisions-so-far / Fog body. `gh issue create --label wayfinder:map`.
- **Child ticket**: an issue linked to the map as a GitHub sub-issue (see [Native relationships](#native-relationships)). Where sub-issues aren't enabled, add the child to a task list in the map body and put `Part of #<map>` at the top of the child body. Labels: `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`). Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: GitHub's **native issue dependencies** — the canonical, UI-visible representation; wire them as described under [Native relationships](#native-relationships). GitHub reports `issue_dependencies_summary.blocked_by` (open blockers only — the live gate). Where dependencies aren't available, fall back to a `Blocked by: #<n>, #<n>` line at the top of the child body. A ticket is unblocked when every blocker is closed.
- **Frontier query**: list the map's open children (`gh issue list --state open`, scoped to the map's sub-issues / task list), drop any with an open blocker (`issue_dependencies_summary.blocked_by > 0`, or an open issue in the `Blocked by` line) or an assignee; first in map order wins.
- **Claim**: `gh issue edit <n> --add-assignee @me` — the session's first write.
- **Resolve**: `gh issue comment <n> --body "<answer>"`, then `gh issue close <n>`, then append a context pointer (gist + link) to the map's Decisions-so-far.
