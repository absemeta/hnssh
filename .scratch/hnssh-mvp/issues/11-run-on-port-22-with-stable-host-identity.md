# 11 — Run HNSSH on public port 22 with stable host identity

**What to build:** An operator can run HNSSH on a single Linux VPS at public TCP port 22 as a hardened non-root process with a stable application host key and a separately verified administrative route.

**Blocked by:** 08 — Harden the SSH and terminal lifecycle; 10 — Expose privacy-preserving operational health.

**Status:** ready-for-agent

- [ ] One startup interface supports a directly bound unprivileged listener for development and an inherited systemd listener for production without changing browsing-session behavior.
- [ ] A systemd socket owns public TCP port 22 with one asynchronous HNSSH process accepting connections and seeing the real peer address.
- [ ] The application runs under a dedicated non-root identity with no login shell, bind capability, or ambient capability.
- [ ] Service hardening includes no-new-privileges, an empty capability bounding set, read-only system files, private temporary storage, restricted writable state, and only required address families.
- [ ] The Ed25519 HNSSH host key is provisioned outside releases, permissioned only for the service identity, fingerprinted, backed up, and required at startup; a missing key never triggers silent regeneration.
- [ ] The operations listener remains loopback-only, port 22 is the only generally public application port, and IPv4/IPv6 behavior is explicit.
- [ ] An operator runbook moves administrative OpenSSH to a key-only private or source-allowlisted route and verifies it from a fresh connection before HNSSH takes port 22.
- [ ] The runbook includes provider-console or rescue recovery for broken firewall, socket, HNSSH, or OpenSSH configuration.
- [ ] A clean Linux VM test verifies socket activation, peer-address visibility, sandboxing, public exposure, local health, administrative access, and stable host fingerprint across restart.
