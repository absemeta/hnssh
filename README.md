# HNSSH

HNSSH is an anonymously accessible, full-screen Hacker News browser served over SSH.
The current runnable slice progressively pages through Hacker News's official top feed, with shared bounded caching, refresh, stale fallback, and local failure states.

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

Use the arrow keys or `j`/`k` to move, Enter to inspect an entry or load the next 30 entries, Escape or Backspace to return, `r` to refresh or retry the feed, `?` for help, and `q` to quit. `HNSSH_LISTEN_ADDRESS` and `HNSSH_HOST_KEY` provide equivalent environment configuration.

The SSH client's first-connection host-authenticity prompt verifies the server and is distinct from visitor authentication. Keep the host key stable: HNSSH deliberately fails startup instead of generating or replacing a missing key.

A client must request an interactive PTY. If automatic PTY allocation is unavailable, reconnect with `ssh -t` (or `ssh -tt` when the client's standard input is not a terminal).

## Verify

```sh
cargo test
```

The acceptance tests connect through a real SSH transport, apply server output to an emulated terminal, and verify anonymous authentication, progressive paging against a fake HTTP endpoint, request sharing and limits, refresh throttling, retries, stale fallback, bounded cache eviction, partial failures, navigation, details, links, help, resize behavior, terminal restoration, clean exit, and the no-PTY error path.
