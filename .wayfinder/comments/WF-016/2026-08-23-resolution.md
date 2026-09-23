# Resolution — 2026-08-23

Approve the provisional direct Entry and Comment-thread interaction without exceptions:

- Enter opens one Entry view containing compact Feed-entry context, available story text or poll options, and its Comment thread; no action chooser remains, and no-comment entries show an explicit empty state.
- `o` exposes the full primary validated external HTTP(S) destination as portable terminal text and reports when none exists. Validated links in story and comment text remain inline as OSC 8 hyperlinks, eliminating the separate links view while preserving reading position.
- Comment subtrees present expanded by default while materializing progressively near the viewport within established bounds. Space, Left/`h`, and Right/`l` affect one subtree; `C` collapses the whole thread to compact author-only rows and `E` restores expanded presentation.
- Structural navigation uses `n` for next sibling, `p` for previous sibling, `P` for parent, and `N` for first child. Only available structural actions appear on the selected Tree-rails header; an unavailable relative changes nothing.
- Selection is anchored to Comment identity through loading, collapse, and resize. Automatic layout changes keep its header visible; explicit Page Up/Page Down traverses wrapped lines without changing selection and may move the header off-screen. Reopening an Entry restores the loaded graph, expansion state, selection, and viewport for the Browsing session.

This completes the interaction decision needed by implementation slice 07.
