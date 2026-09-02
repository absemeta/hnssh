# 11 — Expose privacy-preserving operational health

**What to build:** An operator can determine whether HNSSH is alive, ready, overloaded, or degraded and estimate daily reach without collecting source-address history, usernames, content-level activity, or visitor navigation profiles.

**Blocked by:** 04 — Refresh and page through a resilient top feed; 10 — Protect established browsing sessions from overload.

**Status:** ready-for-agent

- [ ] A loopback-only operations listener exposes liveness, readiness, and aggregate metrics and is not part of the public interface.
- [ ] Liveness reports a responsive runtime; readiness requires valid configuration, loaded host key, listener initialization, and session-runtime initialization but treats Hacker News outage as degradation rather than process failure.
- [ ] Metrics cover active handshakes and browsing sessions, acceptance/rejection/timeouts, upstream status categories and latency, retries, cache hit/stale/coalesced/eviction behavior, rendering/send failures, build information, and daily visitor estimate.
- [ ] The daily visitor estimate feeds a HyperLogLog sketch with a UTC-day-derived HMAC; same-day sources deduplicate, and only the approximate count survives day rollover.
- [ ] Raw addresses, daily/hourly keyed source values, sketches from completed days, and individual session timelines are not retained.
- [ ] Structured logs cover lifecycle and aggregate operational failures while excluding source addresses, SSH usernames, terminal types, navigation events, feed-entry or comment identifiers, titles, URLs, and upstream payloads.
- [ ] Capturing all test logs and metrics while using distinctive source addresses, usernames, and content proves none of those values or reversible hashes escape.
- [ ] Health and telemetry tests cover startup, readiness transition, upstream outage, overload, day rotation, shutdown, and malformed visitor-controlled metadata.
