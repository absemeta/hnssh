# 08 — Keep partial and malformed comment threads usable

**What to build:** A visitor can continue reading a comment thread when individual comments are deleted, dead, missing, stale, failed, duplicated, cyclic, or beyond safe materialization limits.

**Blocked by:** 07 — Make entry links and comment threads direct, navigable, and readable.

**Status:** implemented

- [x] Deleted, dead, missing, unsupported, stale, and retryable-failure comments have distinguishable placeholders where upstream evidence permits the distinction.
- [x] A failed child leaves its siblings and parent usable, and retrying that child does not reset the thread or duplicate loaded nodes.
- [x] Hacker News sibling order and visible parent-child structure remain intact across partial loading, stale refresh, deletion, and failure.
- [x] Path-based cycle detection prevents recursive loops, and duplicate child IDs use the first occurrence without duplicating content.
- [x] One browsing session materializes at most 1,000 nodes for a thread and no more than 64 levels; reaching either limit shows an explicit marker.
- [x] Cancellation during child loading or transport loss aborts session-owned work, while generation checks prevent late results from mutating a different or discarded thread.
- [x] Deterministic real-SSH tests cover null, deleted, dead, failed, stale, cyclic, duplicate, over-depth, over-width, and node-cap fixtures while proving unaffected comments remain navigable.
