# 03 — Browse the live top feed progressively

**What to build:** A visitor can browse the first page of Hacker News's real top feed as entries load independently through a shared process cache, without one slow or malformed entry blocking the page.

**Blocked by:** 02 — Browse a fixture top feed.

**Status:** ready-for-agent

- [ ] Production content comes only from the official Hacker News v0 HTTPS top-feed and item locations; tests can redirect the same HTTP adapter to a controllable fake endpoint.
- [ ] The first 30 IDs retain their upstream array order and appear immediately as stable loading placeholders.
- [ ] Visible item records load progressively with at most eight page-level item requests in flight.
- [ ] Concurrent browsing sessions requesting the same feed or item share one in-flight request and one cached result.
- [ ] Sparse records and unknown JSON fields decode without fabricated values or page-wide failure.
- [ ] Valid `null`, malformed records, and individual request failures become local unavailable or retryable rows while usable siblings remain visible.
- [ ] If no usable top feed has ever loaded, the visitor receives an unavailable screen with a retry action rather than a process failure.
- [ ] End-to-end tests use a fake HTTP endpoint plus real SSH to prove ordering, progressive loading, stable focus, request sharing, sparse data, partial failure, and initial outage behavior.
