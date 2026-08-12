# 12 — Deploy, drain, and roll back safely

**What to build:** An operator can activate or roll back an immutable HNSSH release while retaining host identity, giving connected visitors a bounded graceful close, and restoring service health predictably.

**Blocked by:** 09 — Protect established browsing sessions from overload; 11 — Run HNSSH on public port 22 with stable host identity.

**Status:** ready-for-agent

- [ ] Releases are immutable versioned binaries with checksums, installed beside prior versions and selected atomically without storing runtime state in a release.
- [ ] Deployment verifies local health, a real external SSH browsing journey, and the expected host-key fingerprint before declaring success.
- [ ] SIGTERM stops application acceptance, makes readiness false, prevents new upstream work, and notifies writable browsing sessions of the restart.
- [ ] Active sessions attempt terminal restoration and close, all session-owned and shared child work is cancelled, and the process exits within ten seconds before a 15-second supervisor deadline.
- [ ] The systemd socket retains the public listener during the short process restart gap.
- [ ] Rollback atomically selects the prior release, restarts it, verifies health and SSH, and presents the same host-key fingerprint.
- [ ] Failed deployment and forced-shutdown paths leave no orphan processes, leaked admission permits, writable stale release state, or regenerated host key.
- [ ] Linux integration tests perform upgrade, active/idle/slow-session shutdown, failed health verification, automatic rollback, explicit rollback, and fingerprint comparison.
