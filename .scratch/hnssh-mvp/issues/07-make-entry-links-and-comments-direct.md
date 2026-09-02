# 07 — Make entry links and comment threads direct, navigable, and readable

**What to build:** A visitor can enter the Hacker News view of a selected feed entry immediately, expose its primary external destination with one portable key, and read an expanded, visually structured Comment thread whose selection and viewport follow structural navigation correctly.

**Blocked by:** 06 — Browse comment threads progressively; [Approve the direct entry and comment-reading interaction](../../../.wayfinder/issues/WF-016-approve-direct-entry-comment-interaction.md).

**Status:** complete

This slice supersedes the current Entry-details action chooser and collapsed-by-default thread interaction. The linked wayfinding decision approved the direct Entry view and Tree-rails Comment-thread treatment.

- [x] Enter on an available Feed entry opens one Entry view containing compact entry context, available story text or poll options, and its Comment thread immediately; there is no separate “View comments” or “View links” action menu, and entries without comments show an explicit empty state.
- [x] `o` from the selected Top-feed entry or anywhere in its Entry view exposes the full primary validated external HTTP(S) destination as portable terminal text for terminal-native detection or copying; it never claims to launch the visitor's local browser, and an entry without a destination reports that explicitly.
- [x] `l` opens the relevant validated-link list for the selected Top-feed entry, Entry view, or selected comment, while preserving the visitor's previous selection and viewport on return.
- [x] Comment subtrees are expanded by default, but descendants materialize progressively near the viewport through the established cache, request-concurrency, sibling-batching, depth, and node limits rather than eagerly fetching a complete thread.
- [x] `n` selects the next sibling after the current subtree, `p` the previous sibling, `P` the parent, and `N` the first child, expanding or progressively loading that child when needed; an unavailable relative leaves selection unchanged, and only available structural actions appear on the selected header.
- [x] `C` collapses the whole Comment thread to compact tree rows containing the author but no comment text or metadata, and `E` restores expanded-by-default presentation; Space and Left/Right continue to operate on one subtree, and hiding the selected comment moves selection to its nearest visible ancestor.
- [x] The approved prototype treatment makes nesting, comment boundaries, and the selected comment legible without relying on color, including narrow and deeply nested views; the initial candidate uses `│`/`├`/`└` guides, breathing room between comment blocks, and selected header or gutter emphasis rather than styling the entire body.
- [x] Thread layout records each wrapped comment block's rendered line range, scrolls only enough to keep the selected header visible, and lets page navigation traverse rendered content even when one comment is taller than the viewport.
- [x] Selection is anchored to comment identity across asynchronous loads, retries, expansion, collapse, and resize, so inserted or rewrapped content does not silently focus another comment or move the focus indicator off-screen.
- [x] Leaving and reopening an Entry view restores its loaded thread graph, expansion state, selected comment, and viewport for the lifetime of the Browsing session.
- [x] Help text and README documentation cover the revised Entry view, portable link behavior, structural navigation, whole-thread controls, paging, and fallback behavior.
- [x] Deterministic real-SSH tests cover direct entry, `o`/`l`, no-link and no-comment entries, polls and text stories, bounded expanded loading, every structural key, global and local collapse, long wrapped comments, asynchronous insertion, resize, back-stack restoration, and monochrome-readable focus without asserting exact widget composition.
