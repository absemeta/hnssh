# 13 — Prove the production baseline

**What to build:** Maintainers and operators have repeatable evidence that the complete HNSSH MVP works with representative SSH clients and sustains 200 browsing sessions on an inexpensive Linux VPS while rejecting excess load gracefully.

**Blocked by:** 07 — Keep partial and malformed comment threads usable; 12 — Deploy, drain, and roll back safely.

**Status:** ready-for-agent

- [ ] A repeatable load harness establishes real SSH browsing sessions, drives feed, detail, and progressive comment actions, simulates upstream latency, creates slow readers and resize storms, and causes abrupt disconnects.
- [ ] The production gate runs for at least 30 minutes with 200 established sessions, using an 80% idle-reader and 20% repeated-navigation workload.
- [ ] All 200 sessions remain usable and cached input-to-screen latency is p95 at or below 250 ms.
- [ ] Sustained CPU remains below 70% and RSS below 1.5 GiB on the initial 2-vCPU, 2-GiB Linux target, with no unbounded queue, task, cache, or memory trend.
- [ ] Twenty excess simultaneous arrivals are rejected within their handshake deadline without degrading established sessions.
- [ ] The run produces no panic, leaked task, stuck shutdown, source-address telemetry, or changed host fingerprint and demonstrates recovery after the load ends.
- [ ] Compatibility smoke tests cover current OpenSSH clients in macOS Terminal, iTerm2, and one representative Linux terminal for host verification, credential-free entry, navigation, UTF-8, resize, tiny windows, limited color, normal cleanup, and forced network loss.
- [ ] The measured environment, workload, results, and resource graphs are recorded; if the initial VPS target fails, implementation is tuned or the smallest passing production size is documented before the ticket is complete.
