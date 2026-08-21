# Issue tracker: GitLab

Issues and PRDs for this repo live as GitLab issues. Use the [`glab`](https://gitlab.com/gitlab-org/cli) CLI for all operations.

## Conventions

- **Create an issue**: `glab issue create --title "..." --description "..."`. Use a heredoc for multi-line descriptions. Pass `--description -` to open an editor.
- **Read an issue**: `glab issue view <number> --comments`. Use `-F json` for machine-readable output.
- **List issues**: `glab issue list -F json` with appropriate `--label` filters.
- **Comment on an issue**: `glab issue note <number> --message "..."`. GitLab calls comments "notes".
- **Apply / remove labels**: `glab issue update <number> --label "..."` / `--unlabel "..."`. Multiple labels can be comma-separated or by repeating the flag.
- **Close**: `glab issue close <number>`. `glab issue close` does not accept a closing comment, so post the explanation first with `glab issue note <number> --message "..."`, then close.
- **Merge requests**: GitLab calls PRs "merge requests". Use `glab mr create`, `glab mr view`, `glab mr note`, etc. — the same shape as `gh pr ...` with `mr` in place of `pr` and `note`/`--message` in place of `comment`/`--body`.

Infer the repo from `git remote -v` — `glab` does this automatically when run inside a clone.

## Merge requests as a triage surface

**MRs as a request surface: no.** _(Set to `yes` if this repo treats external merge requests as feature requests; `/triage` reads this flag.)_

When set to `yes`, MRs run through the same labels and states as issues, using the `glab mr` equivalents:

- **Read an MR**: `glab mr view <number> --comments` and `glab mr diff <number>` for the diff.
- **List external MRs for triage**: `glab mr list -F json`, then keep only MRs whose author is not a project member/owner (a contributor's MR, not a maintainer's in-flight work).
- **Comment / label / close**: `glab mr note`, `glab mr update --label`/`--unlabel`, `glab mr close`.

Unlike GitHub, GitLab numbers issues and MRs separately, so `#42` is unambiguous once you know which surface the maintainer means.

## When a skill says "publish to the issue tracker"

Create a GitLab issue.

## When a skill says "fetch the relevant ticket"

Run `glab issue view <number> --comments`.

## Native relationships

GitLab's native blocking link is the canonical, UI-visible representation of a blocking edge; see **Blocking** under Wayfinding operations for how to add one, and note it is a Premium/Ultimate feature.

- **Read the current links before writing**: `glab api projects/:id/issues/:iid/links`. Each entry carries the linked issue's `id` and `iid`, the `link_type`, and the relationship's own `issue_link_id` — three integers, and the reconciliation below needs two of them for different jobs. Page the read to the end; a link past the first page is invisible to the reconciliation, so it gets re-added or left stale.
- **Add only the missing edges.** Re-posting `/blocked_by #<n>` for a link that already exists is at best a no-op note on the issue and at worst a duplicate; reading first is what lets a publish be re-run over a partly-wired set.
- **Remove the links the current declaration dropped**: match the declared blocker against each entry's **`iid`** (the issue number you wrote in the prose), then DELETE that entry's **`issue_link_id`** — `glab api --method DELETE projects/:id/issues/:iid/links/<issue_link_id>`. Not the linked issue's `id`: the endpoint addresses the relationship, not the issue, and passing the wrong integer either fails or deletes a different link. A blocking link the ticket no longer declares still gates it and leaves no residue in the description, so a re-run that only rewrites the prose leaves the ticket falsely blocked.
- **For parents** there is no native link to wire, nothing to reconcile, and the prose is the whole edge — but two prose forms are in use, and a reader has to accept both. `/wayfinder` writes `Part of #<map>` at the top of the description; `/to-tickets` writes a `## Parent` section. Anything that resolves a parent, the frontier query included, should read either.
- **On the free tier, or wherever blocking links are unavailable**, the `Blocked by:` prose line is the whole edge. That is a degradation, not a publish failure.

## Wayfinding operations

Used by `/wayfinder`. The **map** is a single issue with **child** issues as tickets.

- **Map**: a single issue labelled `wayfinder:map`, holding the Notes / Decisions-so-far / Fog body. `glab issue create --label wayfinder:map`. (On GitLab tiers with native epics, an epic may hold the map instead; a labelled issue works everywhere.)
- **Child ticket**: an issue carrying `Part of #<map>` at the top of its description and labels `wayfinder:<type>` (`research`/`prototype`/`grilling`/`task`). Once claimed, the ticket is assigned to the driving dev.
- **Blocking**: GitLab's **native blocking link** — the canonical, UI-visible representation. Add it with the `/blocked_by #<n>` quick action, posted as a note (`glab issue note <child> --message "/blocked_by #<blocker>"`). Native blocking links are a Premium/Ultimate feature; on the free tier (or where unavailable) fall back to a `Blocked by: #<n>, #<n>` line at the top of the description. A ticket is unblocked when every blocker is closed.
- **Frontier query**: `glab issue list -F json` scoped to the map's children, drop any with an open blocker — a native `blocked_by` link to an open issue (`glab api projects/:id/issues/:iid/links`), or an open issue in the `Blocked by` line — or an assignee; first in map order wins.
- **Claim**: `glab issue update <n> --assignee @me` — the session's first write.
- **Resolve**: `glab issue note <n> --message "<answer>"`, then `glab issue close <n>`, then append a context pointer (gist + link) to the map's Decisions-so-far.
