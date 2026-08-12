# 04 — Refresh and page through a resilient top feed

**What to build:** A visitor can load more of the top feed, ask for fresher data, and continue browsing visibly stale content during transient Hacker News failures while global bounds prevent visitor activity from multiplying upstream work.

**Blocked by:** 03 — Browse the live top feed progressively.

**Status:** implemented

- [x] Top-feed pages contain 30 entries and an explicit load-more row retrieves subsequent IDs without eagerly fetching all available metadata.
- [x] `r` requests revalidation, but the same resource revalidates at most once per ten seconds and all waiting browsing sessions share the result.
- [x] Top-feed IDs are fresh for 60 seconds and usable stale for 15 minutes; feed-entry records are fresh for two minutes; comments and poll options are fresh for five minutes; cached items may remain visibly stale for up to 30 minutes after refresh failure.
- [x] Upstream concurrency is globally limited to 16 requests and each request has a five-second connect-plus-response deadline.
- [x] Network failures, timeouts, HTTP 429, and HTTP 5xx receive no more than one jittered retry with bounded `Retry-After` handling; other HTTP 4xx, `null`, and decode failures are not blindly retried.
- [x] The in-memory cache evicts least-recently-used inactive records when either 20,000 entries or an estimated 64 MiB content weight is reached.
- [x] Stale ages, refresh progress, unavailable resources, and retry actions are visible without clearing already usable rows or moving selection unexpectedly.
- [x] Fake-endpoint request counts and real-SSH screen assertions prove paging, coalescing, freshness windows, retry classification, stale fallback, eviction, and recovery.
