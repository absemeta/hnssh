# 09 — Harden the SSH and terminal lifecycle

**What to build:** A browsing session remains safe and bounded under unusual SSH requests, fragmented input, oversized dimensions, slow output, timeout, channel closure, and transport failure.

**Blocked by:** 02 — Browse a fixture top feed; 07 — Make entry links and comment threads direct, navigable, and readable.

**Status:** in-progress

- [x] At most one session channel, one accepted PTY, and one shell request are permitted per connection.
- [x] Exec, subsystem, environment mutation, agent, X11, direct or forwarded TCP channels, and additional channels are explicitly rejected.
- [x] Handshake and PTY/shell establishment each have a ten-second deadline and close with an actionable reason when the channel permits.
- [x] A persistent incremental parser handles escape sequences and UTF-8 split or coalesced across SSH packets with at most 8 KiB pending input.
- [x] Input is limited to 8 KiB/s with a 16 KiB burst, and repeated excess closes only the offending browsing session.
- [ ] Dimensions are clamped to 300 columns by 120 rows, and zero dimensions do not replace the last usable viewport.
- [ ] One persistent Ratatui terminal and a bounded asynchronous output path retain at most the latest pending complete frame for slow readers; superseded animation frames are coalesced.
- [ ] EOF, channel close, send failure, timeout, `q` or Ctrl+C, and transport loss cancel all session-owned tasks; writable graceful paths attempt style reset, cursor show, alternate-screen exit, exit status, EOF, and close.
- [ ] Protocol-level acceptance tests cover unsupported requests, ordering variants, fragmented keys, UTF-8 boundaries, excessive input, huge/zero resize, slow/non-reading clients, abrupt loss, and cleanup.

## Targeted protocol-acceptance increment

The handshake budget covers identification, key exchange, and authentication. A separate absolute setup budget starts at successful authentication, including visitors who never open a channel; only successful PTY/shell establishment disables it. Rejected requests do not restart either budget. Expiry attempts an actionable SSH disconnect, even without a session channel; teardown gets at most two seconds before transport shutdown. Before SSH identification completes, only transport closure is possible.

Russh 0.62.6 spawns its session task after identification and detaches it when `RunningSession` is dropped. A transport shutdown guard therefore also covers server-future cancellation and forced teardown.

Review round 1 reproduced false forwarding acceptance after forwarding → agent → forwarding requests. The original agent workaround was invalid because Russh left global reply state set. HNSSH now pins a locally patched 0.62.6: agent results use channel replies, and global forwarding/cancellation replies consume and honor `want_reply`. The application simply rejects agent forwarding with `Ok(false)`. No dependency versions or transitive pins changed; provenance, upstream backport, tradeoffs, and removal instructions are recorded in `vendor/russh/PATCHES.md`.

Real-SSH tests cover duplicate channels/PTY/shell, request and no-reply rejection, direct TCP/Unix-socket/X11 channels, TCP/Unix-socket forwarding and cancellation, pre-PTY data/resize, resize between PTY and shell, EOF during setup, all setup timeout stages, stalled identification/key exchange/authentication, deadline transitions, healthy browsing past the setup deadline, shutdown, and server-future cancellation. Interleaving regressions cover forwarding → agent → forwarding with both agent reply modes, and no-reply TCP/Unix-socket forwarding/cancellation. A wire-order marker detects unsolicited global failures as well as false success; the tests then prove browsing still works. The public Russh client has no forwarded-TCP channel-open API: that explicit server rejection is source-inspected but not directly exercised by these transport tests.

The input increment adds one persistent decoder task per established browsing session. It preserves fragmented CSI/SS3 keys and UTF-8 until complete, applies a short ambiguity timeout only to a lone Escape key, discards malformed or unterminated input at an 8 KiB pending bound, and resumes parsing subsequent navigation safely.

Ordinary and extended channel data are charged before parsing or discarding to one per-session token bucket refilling at 8 KiB/s with a 16 KiB burst. A first transient overage is dropped; each refill pays excess debt before replenishing tokens, while repeated or sustained excess closes that channel with a nonzero status and leaves concurrent visitors usable. The decoder queue is byte-bounded to 16 KiB, observes actor termination, and drains admitted packets for at most one second before EOF delivers actor stop, preserving an accepted `q` without creating an unbounded teardown wait. Unit tests cover every supported escape-sequence boundary, split UTF-8, malformed/overlong recovery, the pending bound, refill/debt accounting, sustained advancing-time excess, and decoder termination. Real-SSH tests cover fragmented keys and UTF-8 followed by navigation, bounded malformed input recovery, full-burst tolerance, ordinary and extended-data abuse closure, isolation from a concurrent healthy visitor, and `q` immediately followed by EOF.

Bounded output, full graceful terminal restoration across every closure path, idle/lifetime policy, and the remaining hostile-client acceptance coverage remain outside this increment. The overall issue remains open.
