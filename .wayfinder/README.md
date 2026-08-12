# Local Markdown Issue Tracker

This repository has no remote issue tracker, so wayfinding issues live here.

## Issue format

Each issue is a Markdown file under `issues/` with YAML metadata:

- `id`: stable issue identity
- `title`: human-readable issue name
- `status`: `open` or `closed`
- `labels`: issue labels
- `parent`: parent issue id for child issues
- `assignee`: the claim; blank means unclaimed
- `blocked_by`: issue ids that must all be closed
- `context`: optional branch-and-path pointers to supporting assets

The Markdown below the metadata is the issue body. Resolution comments live separately under `comments/<issue-id>/` so answers do not become part of the issue body.

## Wayfinding operations

- **Find the map:** locate the open issue labelled `wayfinder:map`.
- **Find children:** locate issues whose `parent` is the map's id.
- **Find the frontier:** among open children in id order, retain issues with a blank `assignee` and no open issue in `blocked_by`.
- **Claim:** set `assignee` before starting work.
- **Block:** add the blocking issue id to `blocked_by`; this local tracker has no native dependency facility.
- **Resolve:** add a dated resolution comment under `comments/<issue-id>/`, set `status: closed`, and clear `assignee`.
- **Update the map:** append only a linked one-line gist to **Decisions so far**. Keep detailed reasoning in the resolution comment.
- **Rule out of scope:** close the issue, add a resolution comment explaining the boundary, and link its gist under the map's **Out of scope**, not **Decisions so far**.
