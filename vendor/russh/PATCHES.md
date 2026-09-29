# HNSSH's pinned Russh protocol-reply patch

## Provenance and dependency boundary

This is the **russh 0.62.6 crates.io source distribution**, not a dependency upgrade.

- Archive: <https://static.crates.io/crates/russh/russh-0.62.6.crate>
- Archive SHA-256: `b41043523e0edcbd4e31d00903e26f12994f63b21bae9904f7405c1ed92752a5`
- Upstream source commit, recorded in `.cargo_vcs_info.json`: `27554ed84c9af98012bf3c8cb4af2a36b0f6bdec`, package directory `russh`.
- License: Apache-2.0. `LICENSE.txt` is copied from the upstream repository root at that same commit; it was not included in the crate archive.

Only `src/server/encrypted.rs` differs from the published crate's source. This document and `LICENSE.txt` are additions; Cargo's local `.cargo-ok` cache marker is not included. The upstream manifests, examples, tests, and package lockfile are retained verbatim for easy comparison. HNSSH's **root** `Cargo.lock` controls the application build, not the copied package lockfile.

The root manifest pins `=0.62.6` and applies a local `[patch.crates-io]`, so a routine `cargo update` cannot silently replace the fix with another 0.62.x release. The root lockfile changes only Russh's source from registry to local path: **no package versions, transitive pins, crypto dependencies, or feature selections change**.

## Why a local patch is necessary

Russh 0.62.6 routes the boolean result of `Handler::agent_request` through *global* request replies, although agent forwarding is a *channel* request. Returning `false` therefore sends an unsolicited global failure. HNSSH's original workaround (send channel failure, return `true`) was also wrong: global forwarding handlers leave `common.wants_reply` set after responding, so the workaround can send an unsolicited global success. A client with another forwarding request outstanding can observe false acceptance (`Ok(0)`).

The public server API cannot silently clear the global reply flag or suppress the automatic agent reply. Calling `request_success` or `request_failure` to reset it would itself send an incorrect extra reply. Both published versions inspected, 0.62.7 and 0.63.3, retain the agent bug. Upstream has an unreleased fix, but moving to its revision would also import unrelated API/dependency changes. Vendoring this one crate is the narrow backport option; it costs a maintained source copy rather than a broad upgrade or an application-level protocol workaround.

## Changes

1. **Agent forwarding:** backport the two-line production change from upstream [commit 26487fb8753247b507e1feeff81bb3c57657f364](https://github.com/warp-tech/russh/commit/26487fb8753247b507e1feeff81bb3c57657f364), “fix: auth-agent-req@openssh.com replies on channel (#774) (#775)”. Use `channel_success(channel_num)` / `channel_failure(channel_num)` instead of global reply methods. Those methods honor and consume the channel's `want_reply` flag. HNSSH now simply returns `Ok(false)`.
2. **Global reply bookkeeping:** after each TCP/Unix-socket forwarding or cancellation callback, consume `common.wants_reply` with `mem::take` and emit a reply only when requested. Apply the same rule to unknown global requests. This independently fixes unsolicited failures for no-reply global requests and clears stale reply state. Successful dynamic TCP-port replies retain their port payload. This part is a local fix, not attributed to the upstream agent commit.
3. Add a modification notice to the changed source file.

No public API or client implementation is changed.

## Regression coverage and verification

The application acceptance suite (`tests/ssh_lifecycle.rs`) uses the public HNSSH server, real SSH, and a fixture content source:

- `forwarding_agent_forwarding_rejections_do_not_cross_request_scopes`: a rejected global forward, followed by agent forwarding (both reply modes) and another global forward, must never yield acceptance; established browsing remains usable.
- `no_reply_forwarding_and_agent_requests_do_not_emit_unsolicited_replies`: queue interleaved requests in wire order through the public Russh client `Session` API. Test both agent reply modes with no global prefix and with each no-reply TCP/Unix-socket forward/cancellation prefix. A successful PTY acknowledgement is an ordering marker: the later global response must still be pending when that marker arrives. This detects extra global *failures* as well as successes, which an assertion that all forwarding calls are denied would miss. Unexpected channel replies also fail the test. Each connection then establishes a shell, navigates, and quits.

The first regression reproduced `Ok(0)` before the fix. Temporarily restoring the old TCP global reply block (while retaining the agent fix) made the no-reply regression fail with an unsolicited global reply; the corrected block was then restored.

Run from the HNSSH root:

```sh
cargo test
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
git diff --check
```

Path dependencies expose two pre-existing compiler warnings that crates.io normally caps: unused `sec` in `src/keys/format/pkcs5.rs` and unused `MAXIMUM_DECOMPRESSED_PACKET_LEN` with compression disabled. They are not introduced or suppressed by this protocol patch. The root strict-Clippy command still checks HNSSH with warnings denied.

## Update/removal procedure

Compare this directory against an extraction of the checksum-verified 0.62.6 archive; only the source change and additions listed above should differ. When adopting a released upstream version, verify **both** channel-agent replies and global `want_reply` bookkeeping (the agent fix alone is insufficient), then rerun the interleaving/no-reply regressions and full validation. Remove the local patch and vendor directory only after those checks pass. Any dependency or crypto-version upgrade should be reviewed separately rather than bundled into this backport.
