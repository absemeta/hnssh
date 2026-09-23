# 05 — Read safe details and links for every feed-entry variant

**What to build:** A visitor can inspect readable, safe details and links for stories, text stories, jobs, polls, sparse feed entries, and forward-compatible unsupported items without upstream HTML or terminal controls taking over the terminal.

**Blocked by:** 03 — Browse the live top feed progressively.

**Status:** implemented

- [x] The domain model distinguishes stories, jobs, polls, unavailable records, and unsupported future item types while retaining optional upstream fields as optional.
- [x] Detail views present only metadata actually supplied; jobs omit story-only fields and polls show available options and scores.
- [x] Paragraphs, line breaks, emphasis, preformatted code, inline code, and anchors become readable terminal text and styles; unknown elements retain safe descendant text.
- [x] Scripts, styles, unsupported attributes, HTML comments, escape characters, C0/C1 controls, bidi controls, and other terminal-control content cannot reach terminal output.
- [x] Validated inline destinations are OSC 8 hyperlinks on their original story or comment text, while the top-level destination remains available through `o`; HNSSH does not invoke a browser or emit clipboard-control sequences.
- [x] Only absolute HTTP and HTTPS URLs without controls or embedded credentials are usable; malformed or unsupported destinations remain non-actionable.
- [x] Long titles, text, code, and URLs remain within the viewport, and available timestamps show both a non-negative relative age and absolute UTC value.
- [x] Hostile fixture tests pass through the real SSH/terminal seam and prove safe rendering for HTML, Unicode, controls, malformed entities, long content, and malicious URLs.
