# 11 — Expose privacy-preserving operational health

**What to build:** An operator can determine whether HNSSH is alive, ready, overloaded, or degraded and estimate daily reach without collecting source-address history, usernames, content-level activity, or visitor navigation profiles.

**Blocked by:** 04 — Refresh and page through a resilient top feed; 10 — Protect established browsing sessions from overload.

**Status:** complete

- [x] A loopback-only operations listener exposes liveness, readiness, and aggregate metrics and is not part of the public interface.
- [x] Liveness reports a responsive runtime; readiness requires valid configuration, loaded host key, listener initialization, and session-runtime initialization but treats Hacker News outage as degradation rather than process failure.
- [x] Metrics cover active handshakes and browsing sessions, acceptance/rejection/timeouts, upstream status categories and latency, retries, cache hit/stale/coalesced/eviction behavior, rendering/send failures, build information, and daily visitor estimate.
- [x] The daily visitor estimate feeds a HyperLogLog sketch with a UTC-day-derived HMAC; same-day sources deduplicate, and only the approximate count survives day rollover.
- [x] Raw addresses, daily/hourly keyed source values, sketches from completed days, and individual session timelines are not retained.
- [x] Structured logs cover lifecycle and aggregate operational failures while excluding source addresses, SSH usernames, terminal types, navigation events, feed-entry or comment identifiers, titles, URLs, and upstream payloads.
- [x] Capturing all test logs and metrics while using distinctive source addresses, usernames, and content proves none of those values or reversible hashes escape.
- [x] Health and telemetry tests cover startup, readiness transition, upstream outage, overload, day rotation, shutdown, and malformed visitor-controlled metadata.

## Implementation notes

`src/operations.rs` owns a separately bound HTTP listener that rejects non-loopback configuration and exposes only `/livez`, `/readyz`, and `/metrics`. It starts before the SSH runtime in a not-ready state, becomes ready only when `Server::run` initializes session supervision, returns to not-ready before draining, and remains ready with an explicit degraded response during an upstream outage. The listener shuts down with the server and never shares the public SSH protocol surface.

`src/telemetry.rs` defines the complete fixed-label metric schema. Admission permit ownership drives exact active handshake/session gauges; server and output boundaries classify rejections, setup and browsing timeouts, rendering failures, and send failures; and the live Hacker News adapter records bounded upstream status categories, latency, retries, and cache outcomes. Upstream logs occur only on aggregate degraded/recovered transitions and all other lifecycle logs retain the allowlisted, visitor-independent schema.

The only raw source-address boundary remains `src/admission.rs`. It feeds a 1,024-register HyperLogLog sketch with an HMAC derived for the current UTC day from process-only random key material. IPv4-mapped addresses normalize identically, repeated same-day sources deduplicate, a completed sketch is zeroed at rollover, and only its approximate count remains alongside the current estimate. Neither source input nor daily/hourly keyed values can enter the telemetry API.

Unit and real-SSH tests cover UTC rotation, loopback enforcement, pre-runtime readiness, degraded readiness, overload counters, shutdown, fixed metric labels, and exact active gauges. The isolated privacy test drives distinctive source, username, malformed terminal metadata, feed-entry identifier, author, title, and URL values through a real browsing session, then captures all structured events and the metrics endpoint and rejects any raw value or key-shaped token.
