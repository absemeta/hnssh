# 01 — Open an anonymous browsing session

**What to build:** A visitor can connect to HNSSH over real SSH without an account or credentials, receive an interactive Ratatui screen, resize it, and leave cleanly without ever reaching a shell.

**Blocked by:** None — can start immediately.

**Status:** ready-for-agent

- [ ] A development server listens on a configurable unprivileged address and loads an explicitly configured SSH host key.
- [ ] `none` authentication succeeds for any supplied SSH username without offering password, public-key, or keyboard-interactive authentication.
- [ ] One interactive PTY and shell request starts a Ratatui welcome screen through the SSH channel.
- [ ] Terminal resize events update the rendered viewport without reconnecting.
- [ ] Pressing `q` resets styles, shows the cursor, leaves the alternate screen, sends a successful exit status, and closes the channel.
- [ ] A client that does not request an interactive PTY receives a concise reconnection instruction and closes rather than receiving a shell.
- [ ] An automated acceptance test drives a real SSH transport and verifies the visitor-visible screen and graceful close through a terminal emulator/parser.
