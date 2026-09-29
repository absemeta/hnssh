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

The SSH client's first-connection host-authenticity prompt verifies the server and is distinct from visitor authentication. Keep the host key stable: HNSSH deliberately fails startup instead of generating or replacing a missing key.

A client must request an interactive PTY. If automatic PTY allocation is unavailable, reconnect with `ssh -t` (or `ssh -tt` when the client's standard input is not a terminal).

Each connection permits one session channel, one PTY, and one shell request. Commands, subsystems, environment changes, agent/X11 forwarding, and TCP/Unix-socket forwarding are rejected. Identification, key exchange, and authentication have a ten-second deadline; authenticated clients then have ten seconds to establish the PTY and shell. Established browsing sessions are not subject to those setup deadlines.

Russh is pinned with a narrow local protocol-reply patch; see [`vendor/russh/PATCHES.md`](vendor/russh/PATCHES.md) for provenance and the upgrade/removal checklist.

## Verify

```sh
cargo test
```

The test suite drives the real SSH transport against an emulated terminal and fake HTTP endpoint to verify anonymous browsing, bounded acquisition, failures, rich content, comment navigation, terminal lifecycle, and protocol behavior. Focused state tests cover comment-thread depth and loaded-node limits.
