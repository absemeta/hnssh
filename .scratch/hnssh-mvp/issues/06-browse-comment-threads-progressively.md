# 06 — Browse comment threads progressively

**What to build:** A visitor can open a story's comment thread, read its ranked nesting, collapse or expand subtrees, and explicitly load additional siblings without materializing the complete thread.

**Blocked by:** 04 — Refresh and page through a resilient top feed; 05 — Read safe details and links for every feed-entry variant.

**Status:** implemented

- [x] Opening a comment thread loads the first 20 top-level comment IDs in Hacker News sibling order.
- [x] Each loaded comment clearly represents its nesting depth, available author and age, safe formatted text, and child-loading state.
- [x] Right or `l` expands the selected comment, Left or `h` collapses it or moves focus to its parent, and Space toggles the subtree without losing nearby selection.
- [x] Expanding a comment retrieves only its first 20 direct children; an explicit load-more row retrieves subsequent siblings.
- [x] Comment and child requests use the shared cache, coalescing, concurrency, timeout, retry, and stale behavior already established for feed entries.
- [x] Returning to details or the top feed and reopening the thread restores the browsing session's loaded graph, collapse state, selection, and scroll position.
- [x] End-to-end tests drive nested, deep, and wide fixture threads through real SSH and verify upstream order, progressive requests, controls, batching, safe content, and navigation restoration.
