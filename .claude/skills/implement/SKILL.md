---
name: implement
description: "Implement a piece of work based on a spec or set of tickets."
---

Implement the work described by the user in the spec or tickets.

**Your base branch is `main` unless the ticket says otherwise.** A ticket carrying a
`## Delivery` section names a different base — usually a shared integration branch that a
final integrate-and-verify ticket merges. Read it before anything else and substitute it
for `main` throughout this skill: it changes what you rebase onto, what you open the pull
request against, and what "the base branch" means in every check below. Most tickets have
no such section, and then `main` is right.

## Preflight: is this already being done?

**Run this before writing any code**, and report what it says before starting.

An Orca worktree is cut from whatever the base branch pointed at, and other
agents are working the same backlog in their own worktrees. So the work you are
about to start may already exist — open in a PR, or merged into the base branch
since this worktree was created. This has happened: a full implementation of
both halves of an issue was written against a base that was four commits stale,
while one half sat merged on `main` and the other sat in an open PR. Nothing in
the transcript looked wrong until the PR would not merge.

```bash
git fetch origin
git log --oneline HEAD..origin/main          # what moved under you
gh pr list --state open  --limit 30 --json number,title,headRefName,updatedAt
gh pr list --state merged --limit 20 --json number,title,mergedAt
```

Then, for the issue number(s) you were given — say `185`:

```bash
gh pr list --state all --search "185" --json number,title,state,url
gh issue view 185 --json state,title,comments   # closed? linked PR in the comments?
```

Read the results before you start:

- **An open PR already implements it** → stop and tell the user. Offer to review
  that PR, to build on its branch, or to take only the part it does not cover.
  Do not open a competing PR.
- **It landed on the base branch** → stop and tell the user. Re-read the merged
  version; what is left is usually much smaller than the issue, and sometimes
  nothing.
- **A sibling PR touches the same files** → say so, and agree an order before
  starting. Two agents rewriting one module is a merge conflict either way.
- **The base branch has moved** → rebase onto it now, not at PR time. A PR that
  cannot be merged cannot run CI: GitHub builds no merge ref for a conflicting
  PR, so `pull_request` workflows never start and the branch looks untested.

An issue body describing work in the present tense is not evidence the work is
undone — issues are written before the work and are not updated when it lands.
The base branch and the PR list are the evidence.

## Implementing

Use /tdd where possible, at pre-agreed seams.

Run typechecking regularly, single test files regularly, and the full test suite once at the end.

Once done, use /code-review to review the work.

Commit your work to the current branch.

Before opening a PR, `git fetch origin` once more and rebase if the base moved
while you worked — the preflight only proves the base was current when you
started.
