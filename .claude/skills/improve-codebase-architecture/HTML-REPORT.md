# Architecture Review Report

The page itself — scaffold, palette, typography, `.card`, `.badge`, `.callout`,
`.kv`, `.diagram`, `.diagrams`, tables, and the output path — belongs to the
`/html-report` skill. Read its `SKILL.md` and copy its `TEMPLATE.html` first.
This file only covers what's specific to an architecture review.

## Header

Repo name, date, and a one-line `.lede` naming the single deepening you'd do
first. No introduction paragraph — straight into the candidates.

## Candidate card

Each candidate is one `.card`. The diagrams carry the weight; prose is sparse and
uses the glossary terms without ceremony.

- **Title** (`<h3>`) — short, names the deepening: "Collapse the Order intake pipeline".
- **Badge** — recommendation strength, on the title line:
  `Strong` → `.badge.ok` · `Worth exploring` → `.badge.warn` · `Speculative` → `.badge`.
  Add a second `.badge.info` for the dependency category — `in-process`,
  `local-substitutable`, `ports & adapters`, `mock`.
- **Files** — a `.kv` row, paths as `<code translate="no">`.
- **Before / After** — the centrepiece. Two `.diagram` figures inside a
  `.diagrams` grid. Captions say what changed, not what you're looking at.
- **Problem** — one sentence. What hurts.
- **Solution** — one sentence. What changes.
- **Wins** — bullets, ≤6 words each: "Tests hit one interface", "Pricing stops leaking across the seam", "Delete 4 shallow wrappers".
- **ADR conflict** (only if there is one) — a `.callout.warn`, one line.

No paragraphs of explanation. If a diagram needs a paragraph to be understood,
redraw the diagram.

## Diagram patterns

Pick the pattern that fits the candidate, and mix them — if every diagram looks
the same, the report stops being read.

### Mermaid flowchart — the workhorse

For "X calls Y calls Z, and look at the mess." Colour only the leak:

```
flowchart LR
  A[OrderHandler] --> B[OrderValidator]
  B --> C[OrderRepo]
  C -.leak.-> D[PricingClient]
  classDef bad fill:#2a1416,stroke:#f87171,color:#f87171;
  class C,D bad
```

### Mermaid sequence

For round trips. "Before: 6 hops. After: 1." Nothing shows that faster.

### Hand-built boxes and arrows

When Mermaid's layout fights you — particularly the "after" diagram, where one
thick-bordered deep module should visually outweigh its greyed-out internals.
Modules as `<div>`s, arrows as inline `<svg>` over a `position: relative` parent.
Mermaid won't render that with the right weight.

### Cross-section

For layered shallowness. Stack horizontal bands to show the layers a call passes
through. Before: 6 thin layers each doing nothing. After: 1 thick band.

### Mass diagram

For "interface as wide as implementation." Two rectangles per module — interface
surface area, implementation volume. Before: nearly equal heights (shallow).
After: short interface, tall implementation (deep).

Keep before/after diagrams under ~8 nodes and roughly equal in height, so the
pair reads at a glance in the `.diagrams` grid.

## Top recommendation

One `.card` at the end. Candidate name, one sentence on why, an anchor link to
its card. That's it.

## Tone

Plain English, concise — but the architectural nouns and verbs come straight from
the `/codebase-design` skill. Concision is not an excuse to drift.

**Use exactly:** module, interface, implementation, depth, deep, shallow, seam, adapter, leverage, locality.

**Never substitute:** component, service, unit (for module) · API, signature (for interface) · boundary (for seam) · layer, wrapper (for module, when you mean module).

**Phrasings that fit:**

- "Order intake module is shallow — interface nearly matches the implementation."
- "Pricing leaks across the seam."
- "Deepen: one interface, one place to test."
- "Two adapters justify the seam: HTTP in prod, in-memory in tests."

**Wins bullets** name the gain in glossary terms: *"locality: bugs concentrate in one module"*, *"leverage: one interface, N call sites"*, *"interface shrinks; implementation absorbs the wrappers"*. Don't write *"easier to maintain"* or *"cleaner code"* — those aren't in the glossary and don't earn their place.

No hedging, no throat-clearing, no "it's worth noting that…". If a sentence could
be a bullet, make it a bullet. If a bullet could be cut, cut it. If a term isn't in
the `/codebase-design` glossary, reach for one that is before inventing a new one.
