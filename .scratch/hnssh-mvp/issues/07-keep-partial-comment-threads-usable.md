# 07 — Keep partial and malformed comment threads usable

**What to build:** A visitor can continue reading a comment thread when individual comments are deleted, dead, missing, stale, failed, duplicated, cyclic, or beyond safe materialization limits.

**Blocked by:** 06 — Browse comment threads progressively.

**Status:** ready-for-agent

- [ ] Deleted, dead, missing, unsupported, stale, and retryable-failure comments have distinguishable placeholders where upstream evidence permits the distinction.
- [ ] A failed child leaves its siblings and parent usable, and retrying that child does not reset the thread or duplicate loaded nodes.
- [ ] Hacker News sibling order and visible parent-child structure remain intact across partial loading, stale refresh, deletion, and failure.
- [ ] Path-based cycle detection prevents recursive loops, and duplicate child IDs use the first occurrence without duplicating content.
- [ ] One browsing session materializes at most 1,000 nodes for a thread and no more than 64 levels; reaching either limit shows an explicit marker.
- [ ] Cancellation during child loading, transport loss, and leaving the thread cannot leave orphan work that later mutates discarded navigation state.
- [ ] Deterministic real-SSH tests cover null, deleted, dead, failed, stale, cyclic, duplicate, over-depth, over-width, and node-cap fixtures while proving unaffected comments remain navigable.
