---
name: to-tickets
description: Break a plan, spec, or the current conversation into a set of tracer-bullet tickets, each declaring its blocking edges, published to the configured tracker — edges as text in one file per ticket locally, or native blocking links on a real tracker.
---

# To Tickets

Break a plan, spec, or conversation into a set of **tickets** — tracer-bullet vertical slices, each declaring the tickets that **block** it.

The issue tracker and triage label vocabulary should have been provided to you — run `/setup-matt-pocock-skills` if not.

## Process

### 1. Gather context

Work from whatever is already in the conversation context. If the user passes a reference (a spec path, an issue number or URL) as an argument, fetch it and read its full body and comments.

### 2. Explore the codebase (optional)

If you have not already explored the codebase, do so to understand the current state of the code. Ticket titles and descriptions should use the project's domain glossary vocabulary, and respect ADRs in the area you're touching.

Look for opportunities to prefactor the code to make the implementation easier. "Make the change easy, then make the easy change."

### 3. Draft vertical slices

Break the work into **tracer bullet** tickets.

<vertical-slice-rules>

- Each slice cuts a narrow but COMPLETE path through every layer (schema, API, UI, tests) — vertical, NOT a horizontal slice of one layer
- A completed slice is demoable or verifiable on its own
- Each slice is sized to fit in a single fresh context window
- Any prefactoring should be done first

</vertical-slice-rules>

Give each ticket its **blocking edges** — the other tickets that must complete before it can start. A ticket with no blockers can start immediately.

**Wide refactors are the exception to vertical slicing.** A **wide refactor** is one mechanical change — rename a column, retype a shared symbol — whose **blast radius** fans across the whole codebase, so a single edit breaks thousands of call sites at once and no vertical slice can land green. Don't force it into a tracer bullet; sequence it as **expand–contract**. First expand: add the new form beside the old so nothing breaks. Then migrate the call sites over in batches sized by blast radius (per package, per directory), each batch its own ticket blocked by the expand, keeping CI green batch to batch because the old form still exists. Finally contract: delete the old form once no caller remains, in a ticket blocked by every migrate batch.

**Slices that can't be green alone share an integration branch.** Sometimes no ordering makes each slice independently green. The batches of a wide refactor are one case, but so is any set where several slices edit the same files, or where the later slices can't be verified until an earlier one exists. Then keep the sequence, but base every slice on a shared **integration branch** instead of `main`, and have them all block a final **integrate-and-verify** ticket that merges the branch. Green is promised only there.

Decide this **once for the whole set**, and ask rather than assume — it is the exception, not the norm. Raise it in the quiz below, naming the slices that forced it:

> #447, #448 and #449 all edit the same two files, and the gating slices can't be verified until #443 exists — so they can't each be green alone. Share an integration branch that the final ticket merges, or independent pull requests to `main`?

If the user picks independent pull requests, the tickets carry **no `## Delivery` section at all**, and the default applies. Only a shared branch puts one on every slice — see the template below.

### 4. Quiz the user

Present the proposed breakdown as a numbered list. For each ticket, show:

- **Title**: short descriptive name
- **Blocked by**: which other tickets (if any) must complete first
- **What it delivers**: the end-to-end behaviour this ticket makes work

Ask the user:

- Does the granularity feel right? (too coarse / too fine)
- Are the blocking edges correct — does each ticket only depend on tickets that genuinely gate it?
- Should any tickets be merged or split further?
- **Only if some slice cannot be green on its own:** name the slices that force it, and ask whether they share an integration branch or open independent pull requests to `main`. Do not ask otherwise — independent pull requests are the norm and need no discussion.

Iterate until the user approves the breakdown.

### 5. Publish the tickets to the configured tracker

Publish the approved tickets. **How** depends on the tracker `/setup-matt-pocock-skills` configured — the tickets are the same either way, only the shape of the blocking edges changes:

- **Local files** → write one file per ticket under `.scratch/<feature-slug>/issues/<NN>-<slug>.md`, numbered from `01` in dependency order (blockers first). Each file's "Blocked by" lists the numbers/titles it depends on. Use the per-ticket file template below — one ticket per file, never a single combined file.
- **A real issue tracker (GitHub, Linear, …)** → publish one issue per ticket in dependency order (blockers first) so each ticket's blocking edges can reference real identifiers. Write the prose sections **and**, where the platform has native blocking / parent-child relationships, those too — step 6 below. Where the platform has neither, the prose sections are the whole edge. Apply the `ready-for-agent` triage label unless instructed otherwise — the tickets are agent-grabbable by construction.

Work the **frontier**: any ticket whose blockers are all done. For a purely linear chain that means top to bottom.

Do NOT close or modify any parent issue.

<local-ticket-template>

# <NN> — <Ticket title>

**What to build:** the end-to-end behaviour this ticket makes work, from the user's perspective — not a layer-by-layer implementation list.

**Blocked by:** the numbers/titles of the tickets that gate this one, or "None — can start immediately".

**Parent:** the spec or issue this set was broken out of. Omit the line entirely when there was no source spec — this file reference is the only parent edge a local tracker can express, so leaving it out where one exists loses it.

**Delivery:** omit this line entirely unless the set shares an integration branch; see the issue template below for what it says when it is present.

**Status:** ready-for-agent

- [ ] Acceptance criterion 1
- [ ] Acceptance criterion 2

</local-ticket-template>

<issue-template>

## Parent

A reference to the parent issue on the tracker (if the source was an existing issue, otherwise omit this section).

## What to build

The end-to-end behaviour this ticket makes work, from the user's perspective — not layer-by-layer implementation.

## Acceptance criteria

- [ ] Criterion 1
- [ ] Criterion 2

## Delivery

OMIT THIS SECTION ENTIRELY unless the set shares an integration branch. Its absence means the default — branch from `main`, pull request into `main` — which is what nearly every ticket wants and what the implementing agent already does. A section restating the default is scenery, and a section that is usually scenery stops being read, which is how a wrong instruction survives being copied onto every ticket in a set.

When the set does share one, every slice ticket carries the following verbatim, with `<integration>` and the final ticket's number filled in:

> This work lands on the shared integration branch **`<integration>`**, not on `main`. Open your pull request **against `<integration>`** — `main` is not this work's merge target, and #\<final\> is what eventually takes the whole branch there.
>
> **Never check `<integration>` out.** Git refuses to have one branch checked out in two worktrees, so "one shared branch" and "one worktree per agent" cannot both be literal. Treat it as a ref you branch from and target, never one you occupy.
>
> ```bash
> git fetch origin
> git worktree add -b <your-branch> <path> origin/<integration>
> # … work, commit …
> git fetch origin && git rebase origin/<integration>
> git push -u origin HEAD
> gh pr create --base <integration> --fill
> ```
>
> **Rebase onto `origin/<integration>`, never onto `main`.** Rebasing onto `main` silently drops the slices that already landed, and the files they touched are exactly where a conflict will be.
>
> Merge with **"Rebase and merge"**, not squash — the slices are the reviewable unit and #\<final\> preserves them deliberately. Rebase-merge writes **new commit SHAs**, so once your pull request merges your local branch is permanently divergent: fetch and branch fresh rather than reusing it.
>
> A pull request left unopened is the failure this flow exists to prevent. Work sitting in an agent worktree is invisible to every other slice and to #\<final\>, and is indistinguishable from a ticket nobody started.

The integrate-and-verify ticket's own `## Delivery` instead opens by confirming every slice actually landed, because an unopened pull request and an unstarted ticket look identical from the outside:

> ```bash
> git fetch origin
> git log --oneline origin/main..origin/<integration>
> gh pr list --base <integration> --state all
> ```
>
> Chase down anything outstanding before rebasing — a slice discovered after the rebase has to be replayed onto a branch that has already moved. You may check `<integration>` out here; by this point no other worktree should hold it.

## Blocked by

- A reference to each blocking ticket, or "None — can start immediately".

</issue-template>

In either form, avoid specific file paths or code snippets — they go stale fast. Exception: if a prototype produced a snippet that encodes a decision more precisely than prose can (state machine, reducer, schema, type shape), inline it and note briefly that it came from a prototype. Trim to the decision-rich parts — not a working demo, just the important bits.

### 6. Wire the native relationships

Prose is readable, and on the local-files tracker it is the whole edge. On a tracker that has native blocking and parent/child relationships it is not enough on its own: the tracker's UI shows the native links and nothing else, and a tool reading the frontier should not have to parse prose to find an edge. So on a real tracker publish both — every declared blocking edge as a native blocking link, and the parent spec as a native parent/child link. Prose does not become decoration; it stays the readable form, and a tool may still be reading it.

Do it as a second pass, once every issue in the set exists. An edge cannot be wired to an issue that has not been created yet, which is why the first pass only writes bodies.

The re-run this pass survives is a **re-run of the wiring over the set it just published** — the interrupted publish you resume, the edge that failed halfway. Re-publishing an *edited* set is not that: step 5 creates issues, so a second publish creates a second set, and the relationships you reconcile are the new set's while the originals keep theirs. If tickets already exist for this work, say so and edit them rather than publishing again.

Read `docs/agents/issue-tracker.md`, section **Native relationships**, before writing any of them. It holds what this tracker can express and how: the calls, how a relationship is addressed, what makes a re-run idempotent rather than a duplicate, and how to remove an edge the current declaration has dropped. Where it says a relationship has no native form, that is the answer — prose is the whole edge and there is nothing to reconcile. If the document has no such section at all, which happens for a tracker configured from a free-form description, treat that as unknown rather than as none: ask before wiring anything, and record the answer in the document so the next publish does not have to ask. Guessing is not a safe fallback: a relationship addressed wrongly can wire the wrong ticket instead of failing.

Linking a ticket under its parent spec is the one write on the parent that "do NOT close or modify any parent issue" permits. It adds a relationship and changes nothing the parent says.

Where the tracker cannot express a relationship at all, the prose sections are the whole edge and the publish still succeeds. Say once that it degraded, and finish the set.

Verify by re-reading the tracker's relationships for every ticket before reporting the set as published. A frontier tool that flags prose-only edges is a useful cross-check for the blocking edges, but it reads a ticket's parent from prose, so a clean report from it says nothing about whether the parent links landed — only the re-read does.
