# 12 — Run HNSSH on public port 22 with stable host identity

**What to build:** An operator can run HNSSH on a single Linux VPS at public TCP port 22 as a hardened non-root process with a stable application host key and a separately verified administrative route.

**Blocked by:** 09 — Harden the SSH and terminal lifecycle; 11 — Expose privacy-preserving operational health.

**Status:** complete

- [x] One startup interface supports a directly bound unprivileged listener for development and an inherited systemd listener for production without changing browsing-session behavior.
- [x] A systemd socket owns public TCP port 22 with one asynchronous HNSSH process accepting connections and seeing the real peer address.
- [x] The application runs under a dedicated non-root identity with no login shell, bind capability, or ambient capability.
- [x] Service hardening includes no-new-privileges, an empty capability bounding set, read-only system files, private temporary storage, restricted writable state, and only required address families.
- [x] The Ed25519 HNSSH host key is provisioned outside releases, permissioned only for the service identity, fingerprinted, backed up, and required at startup; a missing key never triggers silent regeneration.
- [x] The operations listener remains loopback-only, port 22 is the only generally public application port, and IPv4/IPv6 behavior is explicit.
- [x] An operator runbook moves administrative OpenSSH to a key-only private or source-allowlisted route and verifies it from a fresh connection before HNSSH takes port 22.
- [x] The runbook includes provider-console or rescue recovery for broken firewall, socket, HNSSH, or OpenSSH configuration.
- [x] A clean Linux VM test verifies socket activation, peer-address visibility, sandboxing, public exposure, local health, administrative access, and stable host fingerprint across restart.

## Implementation notes

The executable consumes exactly one native systemd descriptor when a matching `LISTEN_PID` and `LISTEN_FDS=1` are present, validates that descriptor as a listening TCP stream, marks it close-on-exec and nonblocking, and otherwise retains the direct-bind development path. The inherited listener enters the same `Server` accept loop, where the accepted socket's real peer address continues to drive source admission and privacy-preserving reach estimation. Unit, real-server, and Linux subprocess tests cover malformed activation input, direct fallback, inherited acceptance, and source-policy separation.

`deploy/systemd/hnssh.socket` defines one explicit dual-stack port-22 socket with `Accept=no`; `hnssh.service` runs the asynchronous server as a dedicated non-root identity with an empty capability set and a read-only filesystem sandbox. `docs/production.md` records the separate administrative route, stable root-provisioned application host key, fingerprint/backup checks, IPv4/IPv6 and loopback-only verification, and provider-console recovery sequence.

`deploy/validate-production-topology.sh` now performs the destructive end-to-end rehearsal on a disposable systemd VM, and CI runs it against the release artifact on Ubuntu. The gate proves the administrative route before cutover, native activation, local health, dual-stack/public SSH and loopback-only operations, real-peer source separation, the effective capability and mount sandbox, anonymous OpenSSH entry, missing-key failure, and fingerprint stability across restart. It passed locally on Ubuntu 24.04.5 with systemd 255 on 2026-10-23.
