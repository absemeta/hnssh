# Resolution — 2026-08-23

Use prototype variant A, **Tree rails**, as the production Comment-thread presentation baseline. Its `│`/`├`/`└` ancestry guides, breathing room between expanded comment blocks, literal `>>` focus marker, and selected-header emphasis remained readable in ordinary, narrow, deep, long, and monochrome fixtures.

A collapsed comment becomes one compact row: keep the tree and expansion marker, author, and known-descendant count, but hide comment text and metadata. A `+` marks the count as a lower bound when deeper comments have not materialized. On the selected header, show only structurally available navigation helpers using `P` for parent, `n` for next sibling, `p` for previous sibling, and `N` for first child.

The disposable primary source is commit `991c32c` on branch `prototype/readable-expanded-comments`; it must be rewritten against production state rather than promoted directly.
