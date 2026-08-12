# 02 — Browse a fixture top feed

**What to build:** A visitor can use the real SSH interface to browse deterministic fixture feed entries, inspect one entry, consult help, and return to the same place in the top feed.

**Blocked by:** 01 — Open an anonymous browsing session.

**Status:** implemented

- [x] A replaceable content adapter supplies deterministic top-feed fixtures without coupling browsing-session state to the fixture implementation.
- [x] The top feed shows authoritative fixture rank plus each available title, source, score, author, age, and comment count; absent metadata is omitted.
- [x] Up/Down and `j`/`k`, Page Up/Page Down, `g`, and `G` move through the current view with stable selection.
- [x] Enter opens the selected feed entry, Escape or Backspace returns, and the previous selection and scroll position are restored.
- [x] `?` opens a complete help view and returns to the prior view without losing state.
- [x] A terminal below 60 columns by 16 rows shows an actionable resize view while still allowing help and quit.
- [x] Selection does not jump when fixture entries transition from loading placeholders to loaded content.
- [x] End-to-end SSH tests verify navigation, detail, help, back-stack restoration, resize behavior, and clean disconnect by inspecting an emulated terminal screen.
