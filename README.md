# HNSSH

HNSSH is an anonymously accessible, full-screen Hacker News browser served over SSH.
The current runnable slice progressively pages through Hacker News's official top feed, with shared bounded caching, refresh, stale fallback, local failure states, safely rendered rich Entry views, portable validated links, poll options, and readable progressively materialized Comment threads.

## Run locally

Generate a persistent development host key once:

```sh
ssh-keygen -t ed25519 -N '' -f .hnssh-dev-host-key
```

Start HNSSH on an unprivileged address:

```sh
cargo run -- \
  --listen-address 127.0.0.1:2222 \
  --host-key .hnssh-dev-host-key
```

Then connect without a visitor password or key:

```sh
ssh -p 2222 localhost
```

Use the arrow keys or `j`/`k` to move and Enter to open a Feed entry directly. An Entry view combines compact entry context, available story text or poll options, and its Comment thread; Page Up/Page Down traverses rendered content. Press `o` to expose the entry's primary validated external destination as an OSC 8 hyperlink for supported terminals, with the plain URL retained for copying. Validated links in story text and comments are clickable in place with Cmd/Ctrl-click, so there is no separate links view. Escape or Backspace returns, `r` refreshes or retries the feed or selected comment, `?` opens help, and `q` or Ctrl+C quits.

Comment subtrees are expanded by default while descendants materialize progressively near the viewport in sibling batches of 20. Down enters the Comment thread, and Up from its first row returns to the Entry context at the top. Use `n`/`p` for the next/previous sibling comment, `P` for parent, and `N` for first child; available structural actions appear on the selected comment header. Left or `h` collapses or moves to a parent, Right or `l` expands, Space toggles one subtree, and `C`/`E` collapses/expands the whole thread. Collapsed comments retain a compact author row and a known-descendant count while hiding their text and metadata; `+` marks a lower bound when deeper comments have not materialized. `HNSSH_LISTEN_ADDRESS` and `HNSSH_HOST_KEY` provide equivalent environment configuration.

HNSSH admits at most 32 handshakes and 200 established browsing sessions by default; excess arrivals receive a short busy response without displacing current visitors. One transformed network source may hold eight connections and uses a 30-attempt-per-minute token bucket with a burst of ten. `--max-handshakes`, `--max-sessions`, `--max-source-connections`, `--max-source-buckets`, `--source-attempts-per-window`, `--source-attempt-burst`, and `--source-attempt-window-seconds` (and corresponding `HNSSH_*` environment variables shown by `--help`) tune these limits. Raw peer addresses are normalized and immediately HMAC-transformed with an ephemeral hourly-derived key; at most 4,096 short-lived opaque buckets exist in memory by default, and neither addresses nor keyed values enter logs, metrics, or persistence. Panic payloads never reach operational output. A process-wide hook is installed through the production startup wrapper before tracing or the Tokio runtime is constructed, and performs one payload-free low-level stderr write that avoids tracing/stderr locks so a panicking writer cannot cause recursion. After unwinding, scoped-task join failures emit category-only structured warnings (`task.scope`, `task.failure`).

Browsing sessions warn one minute before the default 15-minute input-idle timeout or two-hour maximum lifetime. Any admitted input resets only the idle deadline; the lifetime is absolute. The timeout periods and warning lead are configurable with `--input-idle-seconds`, `--max-session-seconds`, and `--timeout-warning-seconds`.

The SSH client's first-connection host-authenticity prompt verifies the server and is distinct from visitor authentication. Keep the host key stable: HNSSH deliberately fails startup instead of generating or replacing a missing key.

A client must request an interactive PTY. If automatic PTY allocation is unavailable, reconnect with `ssh -t` (or `ssh -tt` when the client's standard input is not a terminal).

Each connection permits one session channel, one PTY, and one shell request. Commands, subsystems, environment changes, agent/X11 forwarding, and TCP/Unix-socket forwarding are rejected. Identification, key exchange, and authentication have a ten-second deadline; authenticated clients then have ten seconds to establish the PTY and shell. Established browsing sessions are not subject to those setup deadlines.

Viewports are capped at 300 columns by 120 rows; zero resize dimensions preserve the last usable size. Slow readers retain only the latest pending complete screen, with SSH-window-aware output and diffs based on the last submitted screen. The browsing actor never waits for output. If TCP writes themselves stall, an independent two-second write-episode budget closes the affected connection and cancels its work, even when SSH callbacks cannot run; partial progress does not extend that budget. This is an output-failure bound, not an idle or session-lifetime policy. Disconnects cancel browsing-session work, and graceful exits attempt terminal restoration before exit status, EOF, and close; restoration cannot be guaranteed after transport loss or against an unwritable peer.

Russh is pinned with a narrow local protocol-reply patch; see [`vendor/russh/PATCHES.md`](vendor/russh/PATCHES.md) for provenance and the upgrade/removal checklist.

## Verify

```sh
cargo test
```

The test suite drives the real SSH transport against an emulated terminal and fake HTTP endpoint to verify anonymous browsing, bounded acquisition, failures, rich content, comment navigation, terminal lifecycle, and protocol behavior. Concurrent acceptance tests hold the exact 32-handshake and 200-session production caps, exercise per-source concurrency and attempt limits, verify clear overload responses and prompt permit reuse, keep admitted visitors navigable during overload, and observe idle/lifetime warnings and closure. Output tests freeze encrypted transport reads with both small SSH windows and large SSH credit plus constrained TCP buffering to verify actual flow-control backpressure, TCP write stalls, screen recovery, resize storms, cancellation, and healthy-visitor isolation. Focused state tests cover comment-thread depth and loaded-node limits.
