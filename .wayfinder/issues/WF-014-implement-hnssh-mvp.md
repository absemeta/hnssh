---
id: WF-014
title: Implement the HNSSH MVP
status: open
labels:
  - ready-for-agent
parent: WF-001
assignee:
blocked_by: []
context:
  - "research/hn-data-access:docs/research/hacker-news-data.md"
  - "research/rust-ssh-ratatui:docs/research/rust-ssh-ratatui.md"
  - "research/linux-port-22:docs/research/linux-port-22.md"
---

## Problem Statement

A visitor currently cannot browse Hacker News through HNSSH: the repository contains only a placeholder Rust executable. Reaching Hacker News still requires a web browser, an account-capable third-party client, or another interface that does not provide the intended anonymous, keyboard-driven terminal experience.

The MVP must turn `ssh <hostname>` into a safe full-screen browser for the Hacker News top feed, feed-entry details, and comment threads. It must remain usable when Hacker News is slow or partially unavailable, correctly handle SSH and terminal lifecycle events, avoid collecting visitor profiles, and run inexpensively on one Linux VPS for 200 simultaneous browsing sessions. The implementation must also be approachable to a senior React/Python engineer learning Rust through runnable vertical milestones.

## Solution

Build a Rust application that terminates SSH itself, accepts credential-free `none` authentication for any supplied SSH username, and gives each visitor an isolated Ratatui browsing session. The application will show Hacker News's authoritative top-feed order, progressively fetch feed entries and comment threads through the official v0 interface, preserve upstream ordering, render a safe terminal-oriented subset of Hacker News content, and share a bounded stale-while-revalidate cache across browsing sessions.

Visitors will navigate with documented keyboard controls, see explicit loading, stale, unavailable, and overload states, inspect validated links as text, and leave without any account or persisted navigation state. Production will use a stable HNSSH host key, a least-privileged systemd service activated by a public port-22 socket, a separate hardened administrative SSH route, bounded resource use, privacy-preserving aggregate telemetry, health checks, graceful shutdown, and a tested rollback and recovery procedure.

## User Stories

1. As a visitor, I want to connect with `ssh <hostname>`, so that I can browse Hacker News without installing a dedicated client.
2. As a visitor, I want HNSSH to require no visitor account, password, or public key, so that entry remains anonymous and low-friction.
3. As a returning visitor, I want the server's SSH host identity to remain stable, so that I am not trained to ignore host-key-change warnings.
4. As a first-time visitor, I want the service documentation to distinguish SSH host verification from visitor authentication, so that an ordinary first-connection trust prompt is not mistaken for an account prompt.
5. As a visitor without an interactive PTY, I want a concise explanation of how to reconnect, so that a failed invocation is actionable.
6. As a visitor, I want the full-screen interface to start as soon as my browsing session is ready, so that I do not encounter a shell or command prompt.
7. As a visitor, I want to see the top feed in Hacker News's authoritative order, so that HNSSH does not invent a different ranking.
8. As a visitor, I want each feed entry to show its rank and available title, source, score, author, age, and comment count, so that I can decide what to inspect.
9. As a visitor, I want metadata that Hacker News did not provide to be omitted rather than fabricated, so that sparse records remain honest.
10. As a visitor, I want jobs, polls, text stories, and other naturally occurring top-feed entries presented sensibly, so that the feed does not break on non-link stories.
11. As a visitor, I want feed entries to appear progressively, so that one slow item does not block the entire feed.
12. As a visitor, I want to move through feed pages without preloading all 500 possible items, so that browsing remains responsive and considerate of Hacker News.
13. As a visitor, I want my selected feed entry to remain selected when nearby entries finish loading, so that asynchronous updates do not move my focus unexpectedly.
14. As a visitor, I want to request a refresh, so that I can ask for newer ranking and metadata without reconnecting.
15. As a visitor, I want refresh requests to be coalesced with other visitors' requests, so that popularity does not multiply upstream traffic.
16. As a visitor, I want stale content to remain available with a visible age marker during a temporary upstream failure, so that an outage does not unnecessarily empty the interface.
17. As a visitor, I want an unavailable state with a retry action when no usable top feed exists, so that failure is clear and recoverable.
18. As a visitor, I want individual failed feed entries to show placeholders, so that partial upstream failure does not make the whole page unusable.
19. As a visitor, I want familiar arrow and `j`/`k` navigation, so that the interface is comfortable in common terminals.
20. As a visitor, I want Page Up, Page Down, top, and bottom shortcuts, so that I can traverse long views efficiently.
21. As a visitor, I want an in-product help view listing every supported key, so that I do not need external instructions while browsing.
22. As a visitor, I want a consistent back action, so that moving between the top feed, details, comment threads, links, and help is predictable.
23. As a visitor, I want navigation state preserved when I return to a previous view, so that inspecting a feed entry does not lose my place.
24. As a visitor, I want terminal resizes reflected without reconnecting, so that HNSSH follows my window.
25. As a visitor with a very small terminal, I want a clear minimum-size message that still lets me get help or quit, so that content is not rendered as an unreadable jumble.
26. As a visitor, I want HNSSH to work without a mouse, so that it remains a terminal-first experience.
27. As a visitor, I want monochrome and limited-color terminals to retain all meaning, so that color is not the only status cue.
28. As a visitor, I want to open a feed entry's detail view, so that I can inspect all available metadata and text.
29. As a visitor, I want paragraphs, emphasis, and code to remain distinguishable, so that Hacker News text is readable without exposing HTML markup.
30. As a visitor, I want unsupported HTML to degrade to readable text, so that historical or unexpected markup does not break rendering.
31. As a visitor, I want validated inline links to remain clickable in story and comment text, so that I can follow them without leaving what I am reading.
32. As a visitor, I want unsafe or malformed URLs excluded from activation and clearly marked, so that upstream content cannot inject terminal behavior.
33. As a visitor, I want long titles, text, code, and URLs wrapped or horizontally constrained, so that they do not corrupt the layout.
34. As a visitor, I want to open a story's comment thread, so that I can read the conversation from the terminal.
35. As a visitor, I want comments shown in Hacker News sibling order, so that HNSSH respects upstream ranking.
36. As a visitor, I want nesting represented clearly, so that parent and child relationships remain understandable.
37. As a visitor, I want to collapse and expand comment subtrees, so that I can control how much of a large thread is visible.
38. As a visitor, I want child comments fetched only when their subtree is expanded, so that opening a large thread does not fan out into unbounded work.
39. As a visitor, I want wide sibling lists loaded in explicit batches, so that I can continue reading without an arbitrary silent cutoff.
40. As a visitor, I want deleted, dead, missing, and temporarily unavailable comments represented distinctly where possible, so that thread shape is preserved without inventing content.
41. As a visitor, I want one failed comment to leave the rest of the thread usable, so that partial failures remain local.
42. As a visitor, I want an explicit limit message if a thread reaches the MVP's depth or loaded-node safety cap, so that bounded behavior is not mistaken for a complete thread.
43. As a visitor, I want malformed cycles or duplicate child references handled safely, so that unexpected upstream data cannot hang the browsing session.
44. As a visitor, I want Hacker News content stripped of terminal control characters, so that titles and comments cannot manipulate my terminal.
45. As a visitor, I want `q` or Ctrl+C to exit cleanly, so that the cursor, style, and alternate screen are restored on normal disconnect.
46. As a visitor, I want network loss or an SSH channel close to cancel work for my browsing session, so that disconnected sessions do not consume server resources.
47. As a visitor, I want unsupported shell, command, subsystem, forwarding, and extra-channel requests rejected, so that HNSSH cannot become a general-purpose SSH service.
48. As a visitor, I want a warning before an idle, maximum-duration, or planned-restart disconnect when the channel is writable, so that the closure is understandable.
49. As a visitor arriving at capacity, I want a short busy response or SSH disconnect reason, so that overload looks intentional rather than like a hung server.
50. As an existing visitor, I want overload to reject excess arrivals instead of displacing my browsing session, so that current sessions remain stable.
51. As a visitor, I want my navigation state discarded at disconnect, so that HNSSH does not create a browsing history or visitor profile.
52. As a privacy-conscious visitor, I want source addresses transformed immediately and never logged or retained raw, so that operating the service does not create an IP history.
53. As a privacy-conscious visitor, I want the daily visitor estimate to be an aggregate approximation rather than an identifier set, so that basic service measurement does not enable tracking.
54. As an operator, I want one shared bounded Hacker News cache, so that 200 browsing sessions do not produce 200 copies of upstream work or content.
55. As an operator, I want global request concurrency, timeout, retry, and cache limits, so that Hacker News slowness cannot exhaust the VPS.
56. As an operator, I want explicit handshake and browsing-session admission limits, so that anonymous connections cannot consume unbounded resources.
57. As an operator, I want short-lived per-source abuse controls without retained source identifiers, so that obvious connection floods are limited without building visitor profiles.
58. As an operator, I want structured operational logs that exclude source addresses, SSH usernames, navigation events, story identifiers, titles, and URLs, so that diagnostics do not become analytics.
59. As an operator, I want aggregate counters and gauges for sessions, rejection, upstream health, cache behavior, errors, and daily visitors, so that I can detect service degradation.
60. As an operator, I want local liveness and readiness checks, so that I can distinguish a dead process from an upstream outage.
61. As an operator, I want the service validated with 200 simultaneous browsing sessions and overload traffic, so that the VPS sizing claim is evidence-based.
62. As an operator, I want slow readers, resize storms, and disconnects included in capacity validation, so that happy-path clients do not hide unbounded queues.
63. As an operator, I want the public SSH host key stored separately from releases, so that upgrades and rollbacks preserve visitor trust.
64. As an operator, I want HNSSH to run as a dedicated non-root identity without ambient capabilities, so that a compromise has a narrow privilege boundary.
65. As an operator, I want public port 22 owned by a systemd socket, so that HNSSH receives only the listener it needs and pending connections have a stable kernel-owned entry point.
66. As an operator, I want administrative OpenSSH moved and verified before HNSSH takes port 22, so that deploying the application does not remove host access.
67. As an operator, I want administrative SSH restricted to keys and a private or source-allowlisted route, so that moving it to a high port is not mistaken for hardening.
68. As an operator, I want a tested provider console or rescue path, so that a bad firewall, socket, or OpenSSH change is recoverable.
69. As an operator, I want graceful shutdown to stop acceptance, notify and close browsing sessions, cancel child work, and exit within a bound, so that restarts do not leave corrupt terminals or orphan tasks.
70. As an operator, I want versioned releases and a documented atomic rollback, so that a bad binary can be reversed without regenerating the host key.
71. As a maintainer, I want local development to use an unprivileged port on macOS and Linux, so that ordinary iteration does not require production privileges.
72. As a maintainer, I want deterministic Hacker News fixtures covering sparse and hostile records, so that behavior does not depend on live upstream data during tests.
73. As a maintainer, I want the primary acceptance tests to drive a real SSH transport and inspect an emulated terminal screen, so that tests verify visitor-visible behavior rather than internal function calls.
74. As a maintainer, I want fetching, caching, navigation, rendering, and SSH lifecycle errors represented explicitly, so that Rust error handling remains local and actionable.
75. As a maintainer learning Rust, I want the implementation delivered as runnable vertical milestones, so that ownership, enums, traits, async work, cancellation, and concurrency are learned in context.

## Implementation Decisions

### Product and interaction

- The MVP has four visitor-visible views: top feed, the combined Entry and Comment-thread view, primary destination, and help. A per-session navigation stack restores selection and scroll state when moving back.
- The global keys are `q` or Ctrl+C to disconnect, `?` for help, `r` to request revalidation, and Escape or Backspace to return. Lists use Up/Down or `k`/`j`, Page Up/Page Down, `g` for the first row, and `G` for the last loaded row. Enter opens the selected feed entry or activates a thread action. In a comment thread, Left or `h` collapses or moves to the parent, Right or `l` expands, and Space toggles the selected subtree.
- The top feed is paged in batches of 30. The current page is fetched progressively with stable placeholders; reaching an explicit “load more” row loads the next batch. HNSSH does not fetch metadata for all top-feed IDs eagerly.
- The detail view presents all meaningful fields that are actually available. Missing optional fields are omitted. Jobs omit story-only metadata; polls show their options and scores when available; text stories display their text.
- Validated links extracted from story and comment content remain inline and use OSC 8 terminal hyperlinks; no separate links view is required. The primary destination remains available through `o` as full text for copying or terminal-native URL handling. HNSSH does not invoke a browser or emit clipboard-control sequences.
- Comment threads preserve every loaded parent-child edge and Hacker News sibling order. Opening a thread loads the first 20 top-level comments. Expanding a comment loads its first 20 direct children, and an explicit row loads subsequent siblings.
- One browsing session may materialize at most 1,000 comment nodes for one thread and at most 64 levels of depth. The interface displays a limit marker when either cap is reached. Path-based cycle detection and first-occurrence deduplication prevent malformed graphs from looping.
- Feed, detail, and thread rows distinguish loading, stale, dead, deleted, missing, unsupported, and retryable-failure states where upstream evidence permits the distinction. A local failure never removes already usable sibling content.
- A terminal smaller than 60 columns by 16 rows receives a resize view rather than the normal layout. Protocol dimensions are clamped to 300 columns by 120 rows before allocation. Zero dimensions do not replace the last usable size.
- Color is decorative. Text labels and symbols carry selection, stale, loading, failure, and nesting meaning. Mouse mode and bracketed paste are not enabled in the MVP.

### Content semantics and safety

- The top-feed array position is the rank. HNSSH never reconstructs or re-sorts ranking from score or time.
- The domain model represents feed entries as explicit variants for stories, jobs, polls, unavailable records, and forward-compatible unsupported item types. Optional upstream fields remain optional; unknown JSON fields are ignored.
- Comment thread structure is built from `kids` in its supplied order. `descendants` is summary metadata only and is never treated as proof that all comments are loaded.
- Hacker News title and text fields are parsed as untrusted HTML. Paragraph breaks, line breaks, emphasis, preformatted code, inline code, and anchors are mapped to terminal text and styles. Unknown elements retain readable descendant text; scripts, styles, comments, and unsupported attributes are discarded.
- HTML entities are decoded, tabs are normalized, and C0/C1 controls, escape characters, bidi control characters, and other terminal-control sequences are stripped except for normalized line breaks. All ANSI sequences originate from HNSSH, never from upstream content.
- Only absolute `http` and `https` destinations with no embedded controls or credentials are exposed as usable links. Invalid or unsupported destinations may be shown as non-actionable text but are never emitted as terminal commands.
- Relative ages use the server clock and never become negative. The detail view also shows an absolute UTC timestamp when an upstream timestamp is available.

### Hacker News acquisition and shared caching

- The official Hacker News v0 HTTPS interface is the only production content source. The MVP uses REST polling and ordinary item reads; SSE, `/updates`, ETags, HTML scraping, and non-first-party ranking interfaces are deferred.
- The acquisition module exposes a small interface for reading the top-feed ID list and one item by ID. It owns HTTP details, sparse JSON decoding, status classification, timeout, retry, and forward-compatible parsing.
- The top-feed ID list is fresh for 60 seconds and may be served stale for up to 15 minutes after its last successful read. Feed-entry records are fresh for two minutes; comments and poll options are fresh for five minutes. Any cached item may be used stale for up to 30 minutes when a refresh fails, with stale status exposed to the visitor.
- A visitor's `r` requests revalidation but does not bypass global protection. Revalidation of the same resource is coalesced and may happen at most once every ten seconds regardless of how many browsing sessions request it.
- Cache misses and stale refreshes for the same feed or item share one in-flight request. Waiting sessions receive the same result without holding application-global locks across network awaits.
- Upstream work is globally limited to 16 concurrent requests, while one page or sibling batch schedules at most eight item requests concurrently. HTTP connect-plus-response time is bounded to five seconds.
- Network failures, timeouts, HTTP 429, and HTTP 5xx receive at most one jittered retry and honor a bounded `Retry-After` when present. Other HTTP 4xx responses, valid `null`, and decode failures are not blindly retried.
- The in-memory cache is bounded by both 20,000 records and an estimated 64 MiB content weight, evicting least-recently-used entries that are not in flight. There is no persistent content cache in the MVP.
- The application begins accepting browsing sessions after configuration, host key, listener, and runtime initialization. If no top feed can be fetched, visitors receive an unavailable screen with retry rather than a startup crash loop.

### Rust module and ownership design

- The code is partitioned into deep modules for domain content, Hacker News acquisition/cache, browsing-session state, terminal rendering/input, SSH runtime, admission/privacy telemetry, configuration, and process lifecycle. Callers and tests use each module's public interface rather than reaching through it to implementation details.
- Shared process state contains the HTTP client, bounded cache, global concurrency/admission controls, privacy-preserving counters, and immutable configuration. It is reference-counted and thread-safe.
- Each browsing session exclusively owns its navigation stack, selection and scroll positions, loaded thread graph, input parser, Ratatui terminal, output state, and cancellation scope. Browsing state is never shared between visitors and is dropped at disconnect.
- A single per-session actor serializes input, resize, timer, data-result, render, and shutdown events. It owns one persistent Ratatui terminal so rendering can diff frames instead of reconstructing a terminal for each draw.
- Async acquisition returns typed results to the session actor through bounded channels. No mutex guard is held across an await, and child tasks cannot outlive their browsing session's cancellation scope.
- Errors are typed by domain: configuration/startup, upstream transport/status/decode, cache availability, SSH protocol/policy, terminal input/output, and session cancellation. Visitor-facing errors are deliberately less detailed than structured operator errors.
- The production dependency lockfile is committed. Maintained compatible releases are selected for Tokio, Russh, Ratatui, HTTP, serialization, HTML parsing, terminal input parsing, cancellation, structured tracing, and metrics; version numbers are not embedded in this issue.

### SSH and terminal runtime

- Russh is the embedded SSH implementation. HNSSH uses Russh directly rather than adopting SSHUI's application lifecycle, allowing explicit admission, PTY acknowledgement, bounded output, cancellation, and protocol policy.
- `none` authentication is accepted for any supplied SSH username. The username is discarded after policy evaluation and is never used as visitor identity, a metric label, or a log field. Password, public-key, and keyboard-interactive authentication are not offered for visitor access.
- Only one `session` channel, one accepted PTY request, and one shell request are allowed per connection. Exec, subsystem, environment mutation, agent, X11, direct/forwarded TCP channels, and additional channels are rejected.
- A connection without a PTY or shell request receives a concise interactive-terminal requirement when possible and then closes. Handshake and PTY/shell establishment each have a ten-second deadline.
- SSH channel data is treated as a byte stream. A persistent incremental parser handles fragmented and coalesced escape sequences and UTF-8. Its pending buffer is bounded to 8 KiB, and inbound data is limited to 8 KiB/s with a 16 KiB burst before repeated abuse causes disconnect.
- One bounded output path connects Ratatui's synchronous draw to asynchronous SSH flow control. At most the latest complete pending frame is retained for a slow reader; superseded animation frames are coalesced instead of queued.
- Rendering is event-driven. Loading animation is capped at four frames per second, and no idle browsing session redraws solely to update every age label each second.
- HNSSH enters the alternate screen, applies its own style reset, and hides the cursor after shell acceptance. Every graceful exit path attempts reset, cursor show, alternate-screen exit, exit status, EOF, and close in that order. Abrupt transport loss is recognized as a limitation: cleanup bytes cannot be delivered after the connection is gone.
- The production host key is Ed25519, generated once outside the release artifact, permissioned for only the HNSSH service identity, persisted across releases, backed up securely, and published to operators as a fingerprint. HNSSH never generates a replacement key merely because one is missing at startup.
- Default limits are 32 concurrent SSH handshakes and 200 established browsing sessions. The 201st session is rejected without disturbing established sessions. Per transformed source, defaults are eight simultaneous connections and 30 connection attempts per minute with a burst of ten; all limits are operator-configurable.
- Browsing sessions have a 15-minute input-idle timeout and a two-hour absolute lifetime. A writable session receives a one-minute warning before a timeout where practical.

### Privacy and observability

- A raw peer address exists only long enough to normalize it and calculate keyed values needed for admission and the daily visitor estimate. It is never written to logs, metrics, errors, or persistent storage.
- Per-source admission uses an hourly derived HMAC value held only in memory. Buckets expire promptly after their rate-limit window; neither raw addresses nor keyed values survive process restart or enter telemetry.
- The daily visitor estimate feeds a HyperLogLog sketch with an HMAC derived from a dedicated secret and the UTC date. At day rollover, only the approximate count is emitted; the daily key material and sketch are discarded. No set of source hashes is retained.
- Normal navigation, selected feed entries, viewed comments, links, SSH usernames, terminal types, and individual connection timelines are not logged or counted.
- Structured logs cover startup/shutdown, configuration class, host-key fingerprint, listener state, aggregate overload events, upstream error categories, cache invariant failures, and panics. Logs redact payloads and visitor-controlled protocol metadata.
- Aggregate metrics include active handshakes and browsing sessions, accepted/rejected/timeout totals, upstream requests by status category, latency, retries, cache hit/stale/coalesced/eviction totals, render/send failures, process build information, and the daily visitor estimate. Metrics have no visitor or content identifiers as labels.
- A local-only operations listener exposes liveness, readiness, and metrics. Liveness means the runtime loop is responsive. Readiness means configuration and host key are valid and the SSH listener/session runtime is initialized; Hacker News outage is reported as degraded metrics rather than making systemd restart a healthy process.

### Production topology and operations

- Production is one provider-neutral Linux VPS managed by systemd. A systemd socket unit owns public TCP port 22 with `Accept=no`; the HNSSH process receives the inherited listener and accepts connections itself, preserving the real peer address and HNSSH host identity.
- Local macOS/Linux development and tests use a directly bound unprivileged port. Listener acquisition is hidden behind one startup interface supporting direct bind and the production inherited descriptor without changing session behavior.
- HNSSH runs under a dedicated non-root system identity with no login shell and no bind capability. The service applies `NoNewPrivileges`, an empty capability bounding set, a read-only system filesystem, private temporary storage, restricted writable state, and only the address families needed for SSH, local operations, DNS, and Hacker News HTTPS.
- Administrative OpenSSH moves to a separate high port or private management address before public port 22 is reassigned. It uses public-key authentication, disables password and root login, and is private-network restricted or source-allowlisted by both provider and host firewalls.
- Only HNSSH port 22 is generally public. The operations listener is loopback-only. Administrative access and any provider-specific IPv4/IPv6 requirements are explicit firewall rules, not accidental defaults.
- Before cutover, the operator validates OpenSSH configuration, opens and tests the new administrative route from a fresh connection, keeps an existing session open, verifies both address families, records the HNSSH fingerprint, and tests a provider console or rescue path that does not depend on guest networking.
- Releases are immutable versioned binaries with checksums. Deployment installs a new version beside the old one, atomically selects it, restarts the service, verifies SSH and local health, and retains the previous version for rollback. Runtime state and the host key live outside release directories.
- On SIGTERM, HNSSH stops accepting from its descriptor, marks readiness false, cancels new upstream work, notifies writable browsing sessions, attempts terminal restoration, and closes all sessions within ten seconds. Remaining work is force-cancelled before the 15-second systemd stop deadline. The socket unit retains the public listener during the short restart gap.
- The initial capacity target is a 2-vCPU, 2-GiB Linux VPS. Production readiness requires the load evidence in this spec on that size or a documented increase to the smallest size that passes; the target is not inferred from crate documentation.

### Runnable implementation milestones

1. **SSH lifecycle slice:** accept an anonymous connection on an unprivileged development port, require PTY/shell, render one Ratatui screen, handle resize and `q` or Ctrl+C, and restore the terminal. This introduces Rust ownership, borrowing, traits, async callbacks, and error propagation.
2. **Fixture browsing slice:** browse fixture top-feed entries, details, links, and help over SSH with the final navigation model. This introduces enums, exhaustive matching, per-session state, collections, and pure state transitions.
3. **Live top-feed slice:** fetch and progressively render the official top feed through the bounded shared cache, including sparse records, retry, stale data, and request coalescing. This introduces Serde, `Result`, shared ownership, futures, semaphores, and cancellation.
4. **Content slice:** safely normalize story/job/poll text and links and render detail variants. This introduces domain conversion, iterator-based transformations, Unicode handling, and defensive parsing.
5. **Comment-thread slice:** progressively browse, expand, collapse, paginate, and recover parts of nested comment threads. This introduces recursive source data represented iteratively, graph guards, ownership of loaded state, and bounded concurrency.
6. **Production-behavior slice:** add admission, timeouts, privacy telemetry, health, graceful shutdown, bounded output, and hostile-protocol fixtures. This introduces atomics/channels, task scopes, lifecycle coordination, and resource invariants.
7. **Linux delivery slice:** add socket activation, service sandboxing, stable host-key provisioning, deployment/rollback automation, external smoke tests, and the 200-session load gate. This connects Rust process behavior to production operations without adding high availability.

## Testing Decisions

- A good test asserts externally observable behavior through a module's public interface. Tests must not assert private task layout, lock counts, cache container choice, Ratatui widget composition, or exact log phrasing. Timing assertions use generous bounded outcomes rather than sleeps when a controllable event or clock can be used.
- The primary functional seam is the one approved for this spec: launch the real HNSSH runtime against a controllable fake Hacker News HTTP endpoint, connect through a real SSH transport, send PTY/input/resize/close events, and inspect the resulting virtual terminal screen. This single high seam covers authentication policy, acquisition, cache behavior, navigation, rendering, output, and browsing-session lifecycle.
- Functional tests use a terminal emulator/parser to apply ANSI output and assert visible rows, focus, status, and restoration rather than snapshotting raw escape bytes. Assertions target meaningful regions and labels so harmless styling changes do not rewrite the suite.
- The fake Hacker News endpoint supplies deterministic cases for ordinary link stories, text stories, jobs, polls, absent optional fields, unknown fields/types, HTML and entities, valid and invalid links, deep/wide comments, deleted/dead/null records, cycles, duplicates, reordered siblings, delayed responses, disconnects, malformed JSON, 4xx, 429 with retry guidance, 5xx, and recovery after failure.
- Core visitor journeys cover connect, top-feed progressive loading, paging, stable selection, details, links, comments, collapse/expand, batched siblings, back-stack restoration, refresh coalescing, stale banners, partial failure, retry, help, minimum-size behavior, resize, idle warning, `q` or Ctrl+C, and transport loss.
- SSH policy journeys cover arbitrary supplied usernames with `none` authentication, absent PTY, shell-before/after-resize timing, fragmented/coalesced keys, UTF-8 boundaries, excess input, multiple channels, exec/subsystem/environment/forwarding requests, handshake timeout, channel EOF/close, slow readers, and shutdown during a draw or fetch.
- Acquisition contract tests run the production HTTP adapter against the fake endpoint and assert request paths, timeout, one-retry policy, status/null/decode classification, unknown-field tolerance, ordering, coalescing, stale windows, and global concurrency. They do not call the live Hacker News interface in the required test suite.
- Content-safety tests feed hostile HTML, long Unicode, control characters, escape sequences, bidi controls, malformed entities, nested tags, code, and malicious URLs and assert only safe visible text and validated links reach the terminal model.
- Cache tests assert behavior through reads and observable fake-endpoint request counts: one in-flight fetch per resource, bounded retries, fresh/stale transitions, no global lock stalls, eviction under both caps, and cancellation of waiters without cancellation of still-needed shared work.
- Privacy tests capture every emitted log and metric while connecting from known addresses, supplying distinctive usernames, and visiting distinctive story IDs. The captures must contain none of those values or reversible source hashes. Daily-estimate tests assert same-day deduplication, day rotation, approximate aggregate output, and sketch disposal.
- The separate production validation seam runs the packaged service on a clean Linux VM matching the target VPS. It verifies systemd socket activation on port 22, direct peer-address availability, stable host fingerprint across upgrade/rollback, service sandboxing, local health, firewall exposure, administrative access, signal handling, restart behavior, and recovery instructions.
- The capacity gate runs for at least 30 minutes with 200 established browsing sessions: 80% idle readers and 20% repeatedly navigating cached feed entries and progressively loaded fixture threads. It also includes connection ramp-up, upstream latency, slow readers, resize storms, and abrupt disconnects.
- The capacity gate passes when all 200 sessions remain usable; cached input-to-screen latency is p95 at or below 250 ms; no queue, task, cache, or RSS trend is unbounded; sustained CPU remains below 70%; RSS remains below 1.5 GiB on the 2-GiB target; no panic or data race occurs; and 20 excess simultaneous arrivals are rejected within their handshake deadline without degrading established sessions. If the target VM cannot pass, the implementation or stated minimum VPS size must change before production readiness.
- Shutdown validation starts with active and slow browsing sessions, sends SIGTERM, confirms readiness drops and new application accepts stop, observes restoration/close attempts, verifies all tasks finish inside 15 seconds, and confirms the same host fingerprint after restart.
- Manual compatibility smoke tests cover current OpenSSH clients in macOS Terminal, iTerm2, and one representative Linux terminal for first-connection host verification, credential-free authentication, navigation keys, resize, UTF-8, color degradation, tiny windows, normal cleanup, and forced network loss. Exhaustive terminal certification is not required.
- The repository has no existing automated-test prior art. The closest prior art is the project research on Hacker News data semantics and the upstream Russh Ratatui examples; these inform fixtures and lifecycle coverage but are not copied as production architecture.

## Out of Scope

- Hacker News authentication, voting, posting, flagging, favorites, visitor accounts, and persisted navigation or reading state.
- Dedicated New, Best, Ask, Show, or Jobs feeds. Those item kinds remain supported when they occur naturally in the top feed.
- A general-purpose shell, arbitrary command execution, port forwarding, file transfer, a web interface, a mobile app, or a public HTTP interface.
- Opening a browser process on the visitor's machine, terminal clipboard integration, or dependence on proprietary hyperlink escape sequences.
- Custom ranking, search, content moderation, user profiles, notifications, and navigation analytics.
- Full browser-grade HTML/CSS rendering, images, embedded media, JavaScript, exhaustive historical-markup fidelity, or retrieval of linked web pages.
- Eagerly materializing complete comment threads, removing all resource caps, or promising a point-in-time snapshot across Hacker News item reads.
- Persistent Hacker News content storage, an offline mode after the stale window, SSE synchronization, `/updates`-based replication, or HTML scraping.
- High availability, multiple VPS instances, autoscaling, containers, Kubernetes, zero-disconnect upgrades, dashboards, or elaborate CI/CD.
- Windows development or production support, exhaustive terminal compatibility, mouse interaction, and terminal-specific workarounds not exposed by ordinary compatibility testing.
- Purchasing a hostname, choosing a VPS vendor, or executing the production deployment. Provider-specific setup values may be documented during implementation.
- Retaining raw source addresses, per-source hashes, SSH usernames, visitor profiles, individual browsing-session histories, or content-level usage metrics.

## Further Notes

- “No account, password, or key required” is the accurate visitor promise. HNSSH cannot suppress the stock OpenSSH client's first-host-key authenticity prompt or a warning caused by a lost/rotated host key.
- Hacker News documents no availability, latency, freshness, or retention SLA. Stale and partial states are normal product states, not exceptional implementation leaks.
- The official top-feed response can include jobs and sparse records, and comment threads are unbounded recursive item reads. The feed-entry vocabulary and progressive limits in this spec are deliberate responses to those source semantics.
- The two accepted test seams are the real SSH/fake-Hacker-News functional seam and the packaged Linux production-validation seam. New lower seams should be added only when they expose behavior that cannot be diagnosed economically through those seams.
- This issue is implementation-ready as an MVP contract. Measured dependency incompatibilities or capacity failures may change a decision only if the issue is updated with the replacement behavior and rationale before implementation proceeds past the affected milestone.
