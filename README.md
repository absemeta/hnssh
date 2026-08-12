# HNSSH

HNSSH is an anonymously accessible, full-screen Hacker News browser served over SSH.
The current runnable slice progressively pages through Hacker News's official top feed, with shared bounded caching, refresh, stale fallback, local failure states, safely rendered rich details, validated links, poll options, and lazy bounded comment threads.

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

Use the arrow keys or `j`/`k` to move, Enter to inspect an entry or activate its selected detail action, Escape or Backspace to return, `r` to refresh or retry the feed, `?` for help, and `q` to quit. Comment threads load siblings in batches of 20; use Left to collapse or move to a parent, Right to expand, and Space to toggle a subtree. `HNSSH_LISTEN_ADDRESS` and `HNSSH_HOST_KEY` provide equivalent environment configuration.

The SSH client's first-connection host-authenticity prompt verifies the server and is distinct from visitor authentication. Keep the host key stable: HNSSH deliberately fails startup instead of generating or replacing a missing key.

A client must request an interactive PTY. If automatic PTY allocation is unavailable, reconnect with `ssh -t` (or `ssh -tt` when the client's standard input is not a terminal).

## Verify

```sh
cargo test
```

The test suite drives the real SSH transport against an emulated terminal and fake HTTP endpoint to verify anonymous browsing, bounded acquisition, failures, rich content, comment navigation, terminal lifecycle, and protocol behavior. Focused state tests cover comment-thread depth and loaded-node limits.
