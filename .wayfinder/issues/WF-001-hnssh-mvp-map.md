---
id: WF-001
title: Chart the implementation-ready HNSSH MVP specification
status: open
labels:
  - wayfinder:map
parent:
assignee:
blocked_by: []
context: []
---

## Destination

An implementation-ready MVP specification and sequenced Rust learning path for a publicly deployable HNSSH service, detailed enough to implement in later vertical milestones without reopening product or architectural decisions.

## Notes

- Domain: an anonymous, full-screen Hacker News browser reached with `ssh <hostname>` and served by a Rust application using Ratatui.
- Audience: a senior React/Python full-stack engineer with no Rust experience; teach Rust-specific mental models in context rather than general programming.
- Every decision session should consult `CONTEXT.md` and invoke the grilling and domain-modeling skills. Prototype and research tickets invoke their corresponding skills.
- Prefer established crates for SSH, terminal rendering, and HTTP; use application state, navigation, concurrency, caching, and errors to learn idiomatic Rust.
- Plan as runnable vertical milestones. Production targets Linux, local development targets macOS and Linux, and one inexpensive VPS should be designed and validated for 200 concurrent active sessions with graceful overload.
- Ratatui provides the terminal baseline. Handle PTY, input, resize, restoration, and disconnect correctly, but do not pursue exhaustive terminal compatibility.
- Track privacy-preserving operational metrics, including a daily estimate based on short-lived keyed source-IP hashes; do not retain raw IPs, navigation analytics, or visitor profiles.
- Planning is the scope of this map. Implementation and production deployment are separate efforts.

## Decisions so far

## Not yet specified

- The exact feasibility experiment or benchmark needed to validate 200 concurrent sessions; its useful shape depends on the selected SSH stack, application architecture, and resource model.
- Stack-specific hardening and operational questions that may surface only after the SSH and deployment topology decisions.
- Terminal interaction gaps or follow-up questions exposed by the fixture prototype; these cannot be named until there is something concrete to test.

## Out of scope

- HN authentication, voting, posting, favorites, visitor accounts, and persisted navigation state.
- Dedicated New, Best, Ask, Show, or Jobs feeds; item types appearing naturally in the top feed remain supported.
- A general-purpose shell, web interface, mobile app, or public HTTP API.
- Custom ranking, content moderation, high availability, autoscaling, dashboards, and elaborate CI/CD.
- Windows support, exhaustive terminal compatibility work, and terminal-specific workarounds not exposed by ordinary testing.
- Purchasing a hostname or executing the production deployment; the specification still covers an actionable deployment and operations plan.
