# 09 — Harden the SSH and terminal lifecycle

**What to build:** A browsing session remains safe and bounded under unusual SSH requests, fragmented input, oversized dimensions, slow output, timeout, channel closure, and transport failure.

**Blocked by:** 02 — Browse a fixture top feed; 07 — Make entry links and comment threads direct, navigable, and readable.

**Status:** ready-for-agent

- [ ] At most one session channel, one accepted PTY, and one shell request are permitted per connection.
- [ ] Exec, subsystem, environment mutation, agent, X11, direct or forwarded TCP channels, and additional channels are explicitly rejected.
- [ ] Handshake and PTY/shell establishment each have a ten-second deadline and close with an actionable reason when the channel permits.
- [ ] A persistent incremental parser handles escape sequences and UTF-8 split or coalesced across SSH packets with at most 8 KiB pending input.
- [ ] Input is limited to 8 KiB/s with a 16 KiB burst, and repeated excess closes only the offending browsing session.
- [ ] Dimensions are clamped to 300 columns by 120 rows, and zero dimensions do not replace the last usable viewport.
- [ ] One persistent Ratatui terminal and a bounded asynchronous output path retain at most the latest pending complete frame for slow readers; superseded animation frames are coalesced.
- [ ] EOF, channel close, send failure, timeout, `q` or Ctrl+C, and transport loss cancel all session-owned tasks; writable graceful paths attempt style reset, cursor show, alternate-screen exit, exit status, EOF, and close.
- [ ] Protocol-level acceptance tests cover unsupported requests, ordering variants, fragmented keys, UTF-8 boundaries, excessive input, huge/zero resize, slow/non-reading clients, abrupt loss, and cleanup.
