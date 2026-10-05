use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use crate::browser::{Browser, FeedRowState, FeedStatus, ViewKind};
use crate::content::{
    Comment, ContentFreshness, EntryDetails, FeedEntry, PollOptionStatus, RichText,
};
use crate::content_safety::validated_url;
use crate::thread::{
    CollapsedComments, CommentThread, StructuralActions, ThreadNodeState, ThreadRow,
};

pub(crate) const MIN_COLUMNS: u16 = 60;
pub(crate) const MIN_ROWS: u16 = 16;
const MAX_VISIBLE_GUIDES: usize = 10;

#[derive(Debug, Default)]
pub(crate) struct RenderOutput {
    pub(crate) comment_candidates: Vec<usize>,
    pub(crate) hyperlinks: Vec<TerminalHyperlink>,
}

#[derive(Debug)]
pub(crate) struct TerminalHyperlink {
    pub(crate) column: u16,
    pub(crate) row: u16,
    pub(crate) text: String,
    pub(crate) destination: String,
}

pub(crate) fn page_len(rows: u16) -> usize {
    usize::from(rows.saturating_sub(3) / 2).max(1)
}

pub(crate) fn navigation_page_len(view: ViewKind, rows: u16) -> usize {
    match view {
        ViewKind::Entry => usize::from(rows.saturating_sub(3)).max(1),
        ViewKind::Feed => page_len(rows),
        ViewKind::Destination | ViewKind::Help => usize::from(rows.saturating_sub(2)).max(1),
    }
}

pub(crate) fn render(
    frame: &mut Frame<'_>,
    browser: &mut Browser,
    size: Rect,
    warning: Option<&str>,
) -> RenderOutput {
    let output = if (size.width < MIN_COLUMNS || size.height < MIN_ROWS)
        && browser.view_kind() != ViewKind::Help
    {
        render_resize(frame, size);
        RenderOutput::default()
    } else {
        match browser.view_kind() {
            ViewKind::Feed => {
                render_feed(frame, browser, size);
                RenderOutput::default()
            }
            ViewKind::Entry => render_entry(frame, browser, size),
            ViewKind::Destination => RenderOutput {
                hyperlinks: render_destination(frame, browser, size),
                ..RenderOutput::default()
            },
            ViewKind::Help => {
                render_help(frame, browser, size);
                RenderOutput::default()
            }
        }
    };
    if let Some(warning) = warning {
        render_timeout_warning(frame, size, warning);
    }
    output
}

fn render_timeout_warning(frame: &mut Frame<'_>, size: Rect, warning: &str) {
    if size.is_empty() {
        return;
    }
    let area = Rect::new(size.x, size.y, size.width, 1);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(warning).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        area,
    );
}

fn render_resize(frame: &mut Frame<'_>, size: Rect) {
    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "Terminal too small",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!(
            "HNSSH needs at least {MIN_COLUMNS} x {MIN_ROWS}; current size is {} x {}.",
            size.width, size.height
        )),
        Line::from("Resize the terminal to browse the top feed."),
        Line::from(""),
        Line::from("? help   q/Ctrl+C quit"),
    ];
    let panel = Paragraph::new(lines)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(Block::default().title(" HNSSH ").borders(Borders::ALL));
    frame.render_widget(panel, size);
}

fn render_feed(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let mut title_status = Vec::new();
    if browser.is_refreshing() {
        title_status.push("refreshing…".to_owned());
    }
    if let Some(ContentFreshness::Stale { .. }) = browser.top_feed_freshness() {
        title_status.push(format!("stale · {} old", cache_age(browser.top_feed_age())));
    }
    if browser.refresh_failed() {
        title_status.push("refresh failed".to_owned());
    }
    let suffix = if title_status.is_empty() {
        String::new()
    } else {
        format!(" · {}", title_status.join(" · "))
    };
    let block = Block::default()
        .title(format!(" HNSSH · Top feed{suffix} "))
        .borders(Borders::ALL);
    let inner = block.inner(body);
    frame.render_widget(block, body);

    let mut lines = Vec::new();
    match browser.feed_status() {
        FeedStatus::Loading => lines.push(Line::from("Loading top feed…")),
        FeedStatus::Unavailable => {
            lines.push(Line::from("Top feed unavailable"));
            lines.push(Line::from("Press r to retry."));
        }
        FeedStatus::Ready if browser.rows().is_empty() => {
            lines.push(Line::from("The top feed is empty."));
        }
        FeedStatus::Ready => {
            let (selected, offset) = browser.feed_position();
            for index in (offset..browser.selectable_feed_len()).take(page_len(size.height)) {
                let marker = if index == selected { ">" } else { " " };
                let style = selected_style(index == selected);
                if let Some(row) = browser.rows().get(index) {
                    lines.push(Line::from(Span::styled(
                        format!("{marker} {}. {}", row.rank, row.title()),
                        style,
                    )));
                    lines.push(Line::from(format!("    {}", feed_metadata(&row.state))));
                } else if browser.has_more() {
                    let count = browser.remaining_count().min(30);
                    lines.push(Line::from(Span::styled(
                        format!("{marker} Load {count} more entries"),
                        style,
                    )));
                    lines.push(Line::from(format!(
                        "    {} entries remain; Enter to load the next page",
                        browser.remaining_count()
                    )));
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
    frame.render_widget(
        Paragraph::new("j/k move · Enter open · o URL · r refresh · ? help · q/Ctrl+C quit")
            .alignment(Alignment::Center),
        footer,
    );
}

struct CommentLineRange {
    node: usize,
    start: usize,
    end: usize,
}

struct EntryHyperlink {
    line: usize,
    column: usize,
    text: String,
    destination: String,
}

struct AnnotatedHyperlink {
    column: usize,
    text: String,
    destination: String,
}

struct AnnotatedLine {
    line: Line<'static>,
    hyperlinks: Vec<AnnotatedHyperlink>,
}

struct EntryLayout {
    lines: Vec<Line<'static>>,
    comments: Vec<CommentLineRange>,
    hyperlinks: Vec<EntryHyperlink>,
}

fn render_entry(frame: &mut Frame<'_>, browser: &mut Browser, size: Rect) -> RenderOutput {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let panel_title = browser.current_entry().map_or_else(
        || " [entry unavailable] ".to_owned(),
        |entry| entry_panel_title(entry.title(), body.width),
    );
    let block = Block::default().title(panel_title).borders(Borders::ALL);
    let inner = block.inner(body);
    frame.render_widget(block, body);

    let Some(entry) = browser.current_entry() else {
        frame.render_widget(Paragraph::new("Entry unavailable"), inner);
        return RenderOutput::default();
    };
    let layout = entry_layout(browser, entry, usize::from(inner.width.max(1)));
    let viewport_height = usize::from(inner.height.max(1));
    let maximum = layout.lines.len().saturating_sub(viewport_height);
    let selected_header = browser.current_thread().and_then(|thread| {
        let selected = thread.selected_node()?;
        layout
            .comments
            .iter()
            .find(|range| range.node == selected)
            .map(|range| range.start)
    });
    browser.reconcile_entry_viewport(selected_header, viewport_height, maximum);
    let scroll = browser.entry_scroll();

    let near_start = scroll.saturating_sub(viewport_height);
    let near_end = scroll.saturating_add(viewport_height.saturating_mul(2));
    let comment_candidates = layout
        .comments
        .iter()
        .filter(|range| range.end >= near_start && range.start <= near_end)
        .map(|range| range.node)
        .collect();
    let hyperlinks = layout
        .hyperlinks
        .iter()
        .filter(|link| link.line >= scroll && link.line < scroll.saturating_add(viewport_height))
        .map(|link| TerminalHyperlink {
            column: inner.x.saturating_add(usize_to_u16(link.column)),
            row: inner
                .y
                .saturating_add(usize_to_u16(link.line.saturating_sub(scroll))),
            text: link.text.clone(),
            destination: link.destination.clone(),
        })
        .collect();
    frame.render_widget(
        Paragraph::new(layout.lines).scroll((usize_to_u16(scroll), 0)),
        inner,
    );
    frame.render_widget(
        Paragraph::new("? help · Enter load/retry · Esc back").alignment(Alignment::Center),
        footer,
    );
    RenderOutput {
        comment_candidates,
        hyperlinks,
    }
}

fn entry_panel_title(title: &str, width: u16) -> String {
    let available = usize::from(width.saturating_sub(4));
    if available == 0 {
        return String::new();
    }
    if Line::from(title).width() <= available {
        return format!(" {title} ");
    }

    let content_width = available.saturating_sub(1);
    let mut trimmed = String::new();
    let mut used: usize = 0;
    for character in title.chars() {
        let character_width = Line::from(character.to_string()).width().max(1);
        if used.saturating_add(character_width) > content_width {
            break;
        }
        trimmed.push(character);
        used = used.saturating_add(character_width);
    }
    format!(" {trimmed}… ")
}

fn entry_layout(browser: &Browser, entry: &FeedEntry, width: usize) -> EntryLayout {
    let mut lines = Vec::new();
    let mut hyperlinks = Vec::new();
    append_annotated_lines(&mut lines, &mut hyperlinks, entry_lines(entry, width), 0);
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Comments",
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));

    let Some(thread) = browser.current_thread() else {
        lines.push(Line::from("No comments available."));
        return EntryLayout {
            lines,
            comments: Vec::new(),
            hyperlinks,
        };
    };
    let rows = thread.rows();
    if rows.is_empty() {
        lines.push(Line::from("No comments available."));
        return EntryLayout {
            lines,
            comments: Vec::new(),
            hyperlinks,
        };
    }

    let selected_position = thread.position();
    let selection_engaged = thread.comment_selection_engaged();
    let mut comments = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        let selected = selection_engaged && position == selected_position;
        match row {
            ThreadRow::Comment {
                node,
                depth,
                guides,
                is_last,
            } => {
                let start = lines.len();
                append_comment(
                    &mut lines,
                    &mut hyperlinks,
                    thread,
                    *node,
                    *depth,
                    guides,
                    *is_last,
                    selected,
                    width,
                );
                comments.push(CommentLineRange {
                    node: *node,
                    start,
                    end: lines.len(),
                });
            }
            ThreadRow::LoadMore {
                depth,
                remaining,
                guides,
                is_last,
                ..
            } => {
                let marker = if selected { ">>" } else { "  " };
                let tree = tree_prefix(*depth, guides, *is_last);
                let count = (*remaining).min(20);
                lines.push(Line::from(Span::styled(
                    format!("{marker} {tree} Load {count} more comments"),
                    selected_style(selected),
                )));
                lines.push(Line::from(""));
            }
            ThreadRow::Marker {
                depth,
                label,
                guides,
                is_last,
            } => {
                let marker = if selected { ">>" } else { "  " };
                let tree = tree_prefix(*depth, guides, *is_last);
                lines.push(Line::from(Span::styled(
                    format!("{marker} {tree} {label}"),
                    selected_style(selected),
                )));
                lines.push(Line::from(""));
            }
        }
    }

    EntryLayout {
        lines,
        comments,
        hyperlinks,
    }
}

fn append_annotated_lines(
    lines: &mut Vec<Line<'static>>,
    hyperlinks: &mut Vec<EntryHyperlink>,
    annotated_lines: Vec<AnnotatedLine>,
    column_offset: usize,
) {
    for annotated in annotated_lines {
        let line = lines.len();
        hyperlinks.extend(
            annotated
                .hyperlinks
                .into_iter()
                .map(|hyperlink| EntryHyperlink {
                    line,
                    column: column_offset.saturating_add(hyperlink.column),
                    text: hyperlink.text,
                    destination: hyperlink.destination,
                }),
        );
        lines.push(annotated.line);
    }
}

#[allow(clippy::too_many_arguments)]
fn append_comment(
    lines: &mut Vec<Line<'static>>,
    hyperlinks: &mut Vec<EntryHyperlink>,
    thread: &CommentThread,
    node_index: usize,
    depth: usize,
    guides: &[bool],
    is_last: bool,
    selected: bool,
    width: usize,
) {
    let Some(node) = thread.node(node_index) else {
        return;
    };
    let marker = if selected { ">>" } else { "  " };
    let tree = tree_prefix(depth, guides, is_last);
    let branch = if !node.expanded() {
        "▶"
    } else if node.has_children() {
        "▼"
    } else {
        "•"
    };
    let (author, metadata) = comment_header(node);
    let helper = if selected {
        structural_helper(thread.structural_actions(node_index))
    } else {
        String::new()
    };
    let header = if node.expanded() {
        format!("{marker} {tree}{branch} {author}{metadata}")
    } else {
        let collapsed = collapsed_comment_label(thread.collapsed_comments(node_index));
        format!("{marker} {tree}{branch} {author}{collapsed}")
    };
    let style = if selected {
        selected_style(true).add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
    };
    lines.push(Line::from(vec![
        Span::styled(header, style),
        Span::styled(helper, Style::default().add_modifier(Modifier::DIM)),
    ]));
    if !node.expanded() {
        return;
    }

    let continuation = continuation_prefix(depth, guides, is_last);
    let body_prefix = format!("   {continuation}   ");
    let body_width = width
        .saturating_sub(Line::from(body_prefix.clone()).width())
        .max(1);
    match node.state() {
        ThreadNodeState::Ready(snapshot) => match &snapshot.value {
            Comment::Available(details) => {
                if let Some(text) = &details.text {
                    let prefix_width = Line::from(body_prefix.clone()).width();
                    for mut annotated in render_rich_text(text, body_width) {
                        let line_number = lines.len();
                        hyperlinks.extend(annotated.hyperlinks.into_iter().map(|hyperlink| {
                            EntryHyperlink {
                                line: line_number,
                                column: prefix_width.saturating_add(hyperlink.column),
                                text: hyperlink.text,
                                destination: hyperlink.destination,
                            }
                        }));
                        annotated
                            .line
                            .spans
                            .insert(0, Span::from(body_prefix.clone()));
                        lines.push(annotated.line);
                    }
                } else {
                    lines.push(Line::from(format!(
                        "{body_prefix}[comment text unavailable]"
                    )));
                }
                if details.omitted_link_count > 0 {
                    for warning in wrap_styled_line(
                        Line::from(format!(
                            "Unsafe or malformed links omitted: {}",
                            details.omitted_link_count
                        )),
                        body_width,
                    ) {
                        let mut warning = warning;
                        warning.spans.insert(0, Span::from(body_prefix.clone()));
                        lines.push(warning);
                    }
                }
            }
            Comment::Deleted { .. }
            | Comment::Dead { .. }
            | Comment::Missing { .. }
            | Comment::Unsupported { .. } => {}
        },
        ThreadNodeState::Loading | ThreadNodeState::Unavailable | ThreadNodeState::Retryable => {}
    }
    lines.push(Line::from(""));
}

fn comment_header(node: &crate::thread::ThreadNode) -> (String, String) {
    match node.state() {
        ThreadNodeState::Loading => (format!("[loading comment {}]", node.id()), String::new()),
        ThreadNodeState::Unavailable => ("[comment unavailable]".to_owned(), String::new()),
        ThreadNodeState::Retryable => (
            "[comment temporarily unavailable]".to_owned(),
            " · Enter to retry".to_owned(),
        ),
        ThreadNodeState::Ready(snapshot) => {
            let author = match &snapshot.value {
                Comment::Available(details) => details
                    .author
                    .clone()
                    .unwrap_or_else(|| "[author unavailable]".to_owned()),
                Comment::Deleted { .. } => "[deleted comment]".to_owned(),
                Comment::Dead { .. } => "[dead comment]".to_owned(),
                Comment::Missing { .. } => "[comment missing]".to_owned(),
                Comment::Unsupported { item_type, .. } => {
                    format!("[unsupported {item_type}]")
                }
            };
            let mut metadata = Vec::new();
            if let Comment::Available(details) = &snapshot.value
                && let Some(published_at) = details.published_at
            {
                metadata.push(relative_age(published_at));
            }
            if snapshot.freshness.is_stale() {
                metadata.push(format!("stale · {} old", cache_age(snapshot.current_age())));
            }
            if node.refreshing() {
                metadata.push("refreshing…".to_owned());
            }
            let metadata = if metadata.is_empty() {
                String::new()
            } else {
                format!(" · {}", metadata.join(" · "))
            };
            (author, metadata)
        }
    }
}

fn collapsed_comment_label(summary: CollapsedComments) -> String {
    match (summary.count, summary.exact) {
        (0, true) => String::new(),
        (0, false) => " · comments collapsed (count unavailable)".to_owned(),
        (count, exact) => {
            let lower_bound = if exact { "" } else { "+" };
            let noun = if count == 1 { "comment" } else { "comments" };
            format!(" · {count}{lower_bound} {noun} collapsed")
        }
    }
}

fn structural_helper(actions: StructuralActions) -> String {
    let mut labels = Vec::new();
    if actions.parent {
        labels.push("parent [P]");
    }
    if actions.next {
        labels.push("next [n]");
    }
    if actions.previous {
        labels.push("prev [p]");
    }
    if actions.child {
        labels.push("child [N]");
    }
    if labels.is_empty() {
        String::new()
    } else {
        format!("   {}", labels.join(" | "))
    }
}

fn tree_prefix(depth: usize, guides: &[bool], is_last: bool) -> String {
    let mut prefix = bounded_guides(depth, guides);
    prefix.push_str(if is_last { "└─" } else { "├─" });
    prefix
}

fn continuation_prefix(depth: usize, guides: &[bool], is_last: bool) -> String {
    let mut prefix = bounded_guides(depth, guides);
    prefix.push_str(if is_last { "  " } else { "│ " });
    prefix
}

fn bounded_guides(depth: usize, guides: &[bool]) -> String {
    if guides.len() <= MAX_VISIBLE_GUIDES {
        return guides
            .iter()
            .map(|has_next| if *has_next { "│ " } else { "  " })
            .collect();
    }
    let retained = 4.min(guides.len());
    let mut prefix = format!("…{depth} ");
    for has_next in &guides[guides.len() - retained..] {
        prefix.push_str(if *has_next { "│ " } else { "  " });
    }
    prefix
}

fn render_destination(
    frame: &mut Frame<'_>,
    browser: &Browser,
    size: Rect,
) -> Vec<TerminalHyperlink> {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let hyperlinks = render_link_destination_panel(
        frame,
        body,
        "Primary destination",
        browser.current_destination(),
        "No primary external destination is available for this entry.",
    );
    frame.render_widget(
        Paragraph::new("Escape/Backspace back · ? help · q/Ctrl+C quit")
            .alignment(Alignment::Center),
        footer,
    );
    hyperlinks
}

fn render_link_destination_panel(
    frame: &mut Frame<'_>,
    area: Rect,
    title: &str,
    link: Option<&crate::content::Link>,
    empty_message: &str,
) -> Vec<TerminalHyperlink> {
    let block = Block::default()
        .title(format!(" HNSSH · {title} "))
        .borders(Borders::ALL);
    let inner = block.inner(area);
    let mut lines = vec![
        Line::from("Cmd/Ctrl-click the URL below, or copy it."),
        Line::from(""),
    ];
    let hyperlinks = if let Some(link) = link {
        let validated_destination = validated_url(&link.destination);
        let destination_style = if validated_destination.is_some() {
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED)
        } else {
            Style::default()
        };
        let destination_lines = wrap_styled_line(
            Line::from(Span::styled(link.destination.clone(), destination_style)),
            usize::from(inner.width.max(1)),
        );
        let destination_chunks = destination_lines.iter().map(line_text).collect::<Vec<_>>();
        lines.extend(destination_lines);
        if should_show_link_label(link) {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                link.label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            )));
        }

        validated_destination.map_or_else(Vec::new, |destination| {
            destination_chunks
                .into_iter()
                .take(usize::from(inner.height.saturating_sub(2)))
                .enumerate()
                .map(|(index, text)| TerminalHyperlink {
                    column: inner.x,
                    row: inner
                        .y
                        .saturating_add(2)
                        .saturating_add(usize_to_u16(index)),
                    text,
                    destination: destination.destination.clone(),
                })
                .collect()
        })
    } else {
        lines.push(Line::from(empty_message.to_owned()));
        Vec::new()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .block(block),
        area,
    );
    hyperlinks
}

fn should_show_link_label(link: &crate::content::Link) -> bool {
    let label = link.label.trim();
    !label.is_empty()
        && label != link.destination
        && !label.starts_with("http://")
        && !label.starts_with("https://")
}

fn line_text(line: &Line<'_>) -> String {
    line.spans.iter().fold(String::new(), |mut text, span| {
        text.push_str(&span.content);
        text
    })
}

fn render_help(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Keyboard help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Up / k                 Previous comment; from first, return to Entry top"),
        Line::from("Down / j               Enter comments or select the next visible row"),
        Line::from("Page Up / Page Down    Page through feed or rendered Entry content"),
        Line::from("g / G                  First / last feed row or Entry content"),
        Line::from("Enter                  Open an Entry; load or retry selected comments"),
        Line::from("Left / h               Collapse a comment or move to its parent"),
        Line::from("Right / l              Expand the selected comment"),
        Line::from("Space                  Toggle the selected comment subtree"),
        Line::from("n / p                  Next / previous sibling comment"),
        Line::from("P / N                  Parent / first child comment"),
        Line::from("C / E                  Collapse / expand the whole Comment thread"),
        Line::from("o                      Expose the primary external destination"),
        Line::from("Linked text            Cmd/Ctrl-click to open in your browser"),
        Line::from("Escape / Backspace     Return to the previous view"),
        Line::from("?                      Open this help"),
        Line::from("r                      Refresh the feed or selected comment"),
        Line::from("q / Ctrl+C             Quit HNSSH"),
    ];
    let panel = Paragraph::new(lines)
        .scroll((browser.current_scroll(), 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" HNSSH · Help ")
                .borders(Borders::ALL),
        );
    frame.render_widget(panel, size);
}

fn entry_lines(entry: &FeedEntry, width: usize) -> Vec<AnnotatedLine> {
    let mut lines = Vec::new();
    push_plain_line(
        &mut lines,
        Line::from(Span::styled(
            entry.title().to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        width,
    );
    let mut context = vec![entry.kind_label().to_owned()];
    if let Some(details) = entry.details() {
        append_compact_metadata(&mut context, details);
    }
    push_plain_line(&mut lines, Line::from(context.join(" · ")), width);
    if let Some(details) = entry.details() {
        if let Some(published_at) = details.published_at {
            push_plain_line(
                &mut lines,
                Line::from(format!("Published: {}", absolute_utc(published_at))),
                width,
            );
        }
        if let Some(text) = &details.text {
            push_plain_line(&mut lines, Line::from(""), width);
            lines.extend(render_rich_text(text, width));
        }
        if details.omitted_link_count > 0 {
            push_plain_line(&mut lines, Line::from(""), width);
            push_plain_line(
                &mut lines,
                Line::from(format!(
                    "Unsafe or malformed links omitted: {}",
                    details.omitted_link_count
                )),
                width,
            );
        }
    }
    if let FeedEntry::Poll { options, .. } = entry {
        push_plain_line(&mut lines, Line::from(""), width);
        push_plain_line(&mut lines, Line::from("Poll options:"), width);
        for option in options {
            let score = option
                .score
                .map(|score| format!(" — {}", points(score)))
                .unwrap_or_default();
            let status = match option.status {
                PollOptionStatus::Available => String::new(),
                PollOptionStatus::Stale { age } => format!(" · stale · {} old", cache_age(age)),
                PollOptionStatus::Unavailable | PollOptionStatus::Retryable => {
                    " · not loaded".to_owned()
                }
            };
            push_plain_line(
                &mut lines,
                Line::from(format!("• {}{score}{status}", option.text)),
                width,
            );
        }
    }
    lines
}

fn push_plain_line(lines: &mut Vec<AnnotatedLine>, line: Line<'static>, width: usize) {
    lines.extend(
        wrap_styled_line(line, width)
            .into_iter()
            .map(|line| AnnotatedLine {
                line,
                hyperlinks: Vec::new(),
            }),
    );
}

fn append_compact_metadata(context: &mut Vec<String>, details: &EntryDetails) {
    if let Some(source) = &details.source {
        context.push(source.clone());
    }
    if let Some(score) = details.score {
        context.push(points(score));
    }
    if let Some(author) = &details.author {
        context.push(author.clone());
    }
    if let Some(published_at) = details.published_at {
        context.push(relative_age(published_at));
    }
    if let Some(comment_count) = details.comment_count {
        context.push(comments(comment_count));
    }
}

fn wrap_styled_line(line: Line<'static>, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut output = Vec::new();
    let mut current = Vec::<Span<'static>>::new();
    let mut current_width: usize = 0;

    for span in line.spans {
        let style = span.style;
        let mut segment = String::new();
        for character in span.content.chars() {
            let character_width = Line::from(character.to_string()).width().max(1);
            if current_width.saturating_add(character_width) > width && current_width > 0 {
                push_segment(&mut current, &mut segment, style);
                output.push(Line::from(std::mem::take(&mut current)));
                current_width = 0;
            }
            segment.push(character);
            current_width = current_width.saturating_add(character_width);
        }
        push_segment(&mut current, &mut segment, style);
    }
    if !current.is_empty() || output.is_empty() {
        output.push(Line::from(current));
    }
    output
}

fn push_segment(spans: &mut Vec<Span<'static>>, segment: &mut String, style: Style) {
    if !segment.is_empty() {
        spans.push(Span::styled(std::mem::take(segment), style));
    }
}

fn render_rich_text(text: &RichText, width: usize) -> Vec<AnnotatedLine> {
    let width = width.max(1);
    let mut output = Vec::new();

    for rich_line in &text.lines {
        let mut source_spans = Vec::new();
        if rich_line.preformatted {
            source_spans.push(("│ ".to_owned(), Style::default(), None));
        }
        for span in &rich_line.spans {
            let mut style = Style::default();
            if span.style.emphasis {
                style = style.add_modifier(Modifier::ITALIC);
            }
            if span.style.strong {
                style = style.add_modifier(Modifier::BOLD);
            }
            if span.style.code {
                style = style.fg(Color::Cyan).add_modifier(Modifier::REVERSED);
            }
            let destination = span
                .link_destination
                .as_deref()
                .and_then(validated_url)
                .map(|url| url.destination);
            if destination.is_some() {
                style = Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::UNDERLINED);
            }
            source_spans.push((span.text.clone(), style, destination));
        }

        let mut current_spans = Vec::new();
        let mut current_hyperlinks = Vec::new();
        let mut current_width = 0usize;
        for (text, style, destination) in source_spans {
            let mut segment = String::new();
            let mut segment_start = current_width;
            for character in text.chars() {
                let character_width = Line::from(character.to_string()).width().max(1);
                if current_width.saturating_add(character_width) > width && current_width > 0 {
                    push_rich_segment(
                        &mut current_spans,
                        &mut current_hyperlinks,
                        &mut segment,
                        segment_start,
                        style,
                        destination.as_deref(),
                    );
                    output.push(AnnotatedLine {
                        line: Line::from(std::mem::take(&mut current_spans)),
                        hyperlinks: std::mem::take(&mut current_hyperlinks),
                    });
                    current_width = 0;
                    segment_start = 0;
                }
                if segment.is_empty() {
                    segment_start = current_width;
                }
                segment.push(character);
                current_width = current_width.saturating_add(character_width);
            }
            push_rich_segment(
                &mut current_spans,
                &mut current_hyperlinks,
                &mut segment,
                segment_start,
                style,
                destination.as_deref(),
            );
        }
        if !current_spans.is_empty() || output.is_empty() || rich_line.spans.is_empty() {
            output.push(AnnotatedLine {
                line: Line::from(current_spans),
                hyperlinks: current_hyperlinks,
            });
        }
    }
    output
}

fn push_rich_segment(
    spans: &mut Vec<Span<'static>>,
    hyperlinks: &mut Vec<AnnotatedHyperlink>,
    segment: &mut String,
    column: usize,
    style: Style,
    destination: Option<&str>,
) {
    if segment.is_empty() {
        return;
    }
    let text = std::mem::take(segment);
    if let Some(destination) = destination {
        hyperlinks.push(AnnotatedHyperlink {
            column,
            text: text.clone(),
            destination: destination.to_owned(),
        });
    }
    spans.push(Span::styled(text, style));
}

fn selected_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn feed_metadata(state: &FeedRowState) -> String {
    match state {
        FeedRowState::Loading => "waiting for Hacker News".to_owned(),
        FeedRowState::Unavailable => "unavailable".to_owned(),
        FeedRowState::Retryable => "temporary failure · retry with r".to_owned(),
        FeedRowState::Ready(snapshot) => {
            let mut label = snapshot
                .value
                .details()
                .map(metadata)
                .filter(|metadata| !metadata.is_empty())
                .unwrap_or_else(|| snapshot.value.kind_label().to_owned());
            if let ContentFreshness::Stale { .. } = snapshot.freshness {
                label.push_str(&format!(
                    " · stale · {} old",
                    cache_age(snapshot.current_age())
                ));
            }
            label
        }
    }
}

fn metadata(details: &EntryDetails) -> String {
    let mut fields = Vec::new();
    if let Some(source) = &details.source {
        fields.push(source.clone());
    }
    if let Some(score) = details.score {
        fields.push(points(score));
    }
    if let Some(author) = &details.author {
        fields.push(author.clone());
    }
    if let Some(published_at) = details.published_at {
        fields.push(relative_age(published_at));
    }
    if let Some(comment_count) = details.comment_count {
        fields.push(comments(comment_count));
    }
    fields.join(" · ")
}

fn points(score: u32) -> String {
    if score == 1 {
        "1 point".to_owned()
    } else {
        format!("{score} points")
    }
}

fn comments(count: u32) -> String {
    if count == 1 {
        "1 comment".to_owned()
    } else {
        format!("{count} comments")
    }
}

fn relative_age(published_at: SystemTime) -> String {
    let age = SystemTime::now()
        .duration_since(published_at)
        .unwrap_or(Duration::ZERO);
    let seconds = age.as_secs();
    if seconds < 60 {
        "just now".to_owned()
    } else if seconds < 60 * 60 {
        quantity(seconds / 60, "minute")
    } else if seconds < 24 * 60 * 60 {
        quantity(seconds / (60 * 60), "hour")
    } else if seconds < 365 * 24 * 60 * 60 {
        quantity(seconds / (24 * 60 * 60), "day")
    } else {
        quantity(seconds / (365 * 24 * 60 * 60), "year")
    }
}

fn absolute_utc(published_at: SystemTime) -> String {
    httpdate::fmt_http_date(published_at).replace("GMT", "UTC")
}

fn cache_age(age: Duration) -> String {
    let seconds = age.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 60 * 60 {
        format!("{}m", seconds / 60)
    } else {
        format!("{}h", seconds / (60 * 60))
    }
}

fn quantity(value: u64, unit: &str) -> String {
    let suffix = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{suffix} ago")
}

fn usize_to_u16(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_panel_title_replaces_the_generic_label_and_fits_the_border() {
        assert_eq!(entry_panel_title("A short title", 40), " A short title ");

        let title = entry_panel_title("abcdefghijk", 12);
        assert_eq!(title, " abcdefg… ");
        assert!(Line::from(title).width() <= 10);
    }

    #[test]
    fn destination_does_not_repeat_a_url_shaped_label() {
        let truncated_url_label = crate::content::Link::new(
            "https://example.com/a-long-path…",
            "https://example.com/a-long-path",
        );
        let descriptive_label =
            crate::content::Link::new("Documentation", "https://example.com/docs");

        assert!(!should_show_link_label(&truncated_url_label));
        assert!(should_show_link_label(&descriptive_label));
    }

    #[test]
    fn wrapped_rich_links_keep_clickable_chunks_at_their_visible_columns() {
        let normalized = crate::content_safety::normalize_html(
            "xx<a href='https://example.com/docs'>abcdef</a>",
        );
        let rendered = render_rich_text(&normalized.text, 5);
        let hyperlinks = rendered
            .iter()
            .flat_map(|line| &line.hyperlinks)
            .map(|link| (link.column, link.text.as_str(), link.destination.as_str()))
            .collect::<Vec<_>>();

        assert_eq!(
            hyperlinks,
            vec![
                (2, "abc", "https://example.com/docs"),
                (0, "def", "https://example.com/docs"),
            ]
        );
    }
}
