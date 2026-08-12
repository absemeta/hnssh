# 09 — Protect established browsing sessions from overload

**What to build:** Existing visitors remain usable when anonymous connection demand exceeds capacity, while excess arrivals, abusive sources, idle sessions, and overlong sessions receive bounded and understandable outcomes.

**Blocked by:** 08 — Harden the SSH and terminal lifecycle.

**Status:** ready-for-agent

- [ ] No more than 32 SSH handshakes and 200 established browsing sessions consume application resources at once.
- [ ] Excess arrivals receive a short busy response or SSH disconnect reason within their handshake deadline and do not displace established browsing sessions.
- [ ] Default per-source limits allow eight simultaneous connections and 30 attempts per minute with a burst of ten, and all admission limits are operator-configurable.
- [ ] A peer address is normalized and transformed immediately for rate limiting; raw addresses are never logged, emitted as metrics, or persisted.
- [ ] Per-source admission uses hourly derived keyed values held only in memory, and expired buckets disappear promptly after their limiting window.
- [ ] Browsing sessions have a 15-minute input-idle timeout and a two-hour absolute lifetime, with a one-minute visitor-visible warning when the channel is writable.
- [ ] Admission permits are released on every handshake error, timeout, channel close, panic, cancellation, and transport-loss path.
- [ ] Concurrent acceptance tests prove exact global and per-source limits, warning/timeout behavior, prompt permit recovery, clear rejection, and continued navigation for established sessions.
