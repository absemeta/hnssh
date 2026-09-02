use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::browser::{Browser, FeedRowState, FeedStatus, ViewKind};
use crate::content::{
    Comment, ContentFreshness, EntryDetails, FeedEntry, PollOptionStatus, RichText,
};
use crate::thread::{CommentThread, StructuralActions, ThreadNodeState, ThreadRow};

pub(crate) const MIN_COLUMNS: u16 = 60;
pub(crate) const MIN_ROWS: u16 = 16;
const MAX_VISIBLE_GUIDES: usize = 10;

pub(crate) fn page_len(rows: u16) -> usize {
    usize::from(rows.saturating_sub(3) / 2).max(1)
}

pub(crate) fn navigation_page_len(view: ViewKind, rows: u16) -> usize {
    match view {
        ViewKind::Entry => usize::from(rows.saturating_sub(4)).max(1),
        ViewKind::Feed | ViewKind::Links => page_len(rows),
        ViewKind::Destination | ViewKind::Help => usize::from(rows.saturating_sub(2)).max(1),
    }
}

pub(crate) fn render(frame: &mut Frame<'_>, browser: &mut Browser, size: Rect) -> Vec<usize> {
    if (size.width < MIN_COLUMNS || size.height < MIN_ROWS) && browser.view_kind() != ViewKind::Help
    {
        render_resize(frame, size);
        return Vec::new();
    }

    match browser.view_kind() {
        ViewKind::Feed => {
            render_feed(frame, browser, size);
            Vec::new()
        }
        ViewKind::Entry => render_entry(frame, browser, size),
        ViewKind::Links => {
            render_links(frame, browser, size);
            Vec::new()
        }
        ViewKind::Destination => {
            render_destination(frame, browser, size);
            Vec::new()
        }
        ViewKind::Help => {
            render_help(frame, browser, size);
            Vec::new()
        }
    }
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
        Line::from("? help   q quit"),
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
        Paragraph::new(
            "j/k move · Enter open · o destination · l links · r refresh · ? help · q quit",
        )
        .alignment(Alignment::Center),
        footer,
    );
}

struct CommentLineRange {
    node: usize,
    start: usize,
    end: usize,
}

struct EntryLayout {
    lines: Vec<Line<'static>>,
    comments: Vec<CommentLineRange>,
}

fn render_entry(frame: &mut Frame<'_>, browser: &mut Browser, size: Rect) -> Vec<usize> {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(size);
    let block = Block::default()
        .title(" HNSSH · Entry ")
        .borders(Borders::ALL);
    let inner = block.inner(body);
    frame.render_widget(block, body);

    let Some(entry) = browser.current_entry() else {
        frame.render_widget(Paragraph::new("Entry unavailable"), inner);
        return Vec::new();
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
    frame.render_widget(
        Paragraph::new(layout.lines).scroll((usize_to_u16(scroll), 0)),
        inner,
    );

    let near_start = scroll.saturating_sub(viewport_height);
    let near_end = scroll.saturating_add(viewport_height.saturating_mul(2));
    let candidates = layout
        .comments
        .iter()
        .filter(|range| range.end >= near_start && range.start <= near_end)
        .map(|range| range.node)
        .collect();

    frame.render_widget(
        Paragraph::new(vec![
            Line::from(
                "j/k linear · n/p siblings · P parent · N child · Space or ←/→ collapse/expand",
            ),
            Line::from(
                "PgUp/PgDn read · C/E all · o destination · l links · Esc back · ? help · q quit",
            ),
        ])
        .alignment(Alignment::Center),
        footer,
    );
    candidates
}

fn entry_layout(browser: &Browser, entry: &FeedEntry, width: usize) -> EntryLayout {
    let mut lines = physical_lines(entry_lines(entry), width);
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
        };
    };
    let rows = thread.rows();
    if rows.is_empty() {
        lines.push(Line::from("No comments available."));
        return EntryLayout {
            lines,
            comments: Vec::new(),
        };
    }

    let selected_position = thread.position();
    let mut comments = Vec::new();
    for (position, row) in rows.iter().enumerate() {
        let selected = position == selected_position;
        match row {
            ThreadRow::Comment {
                node,
                depth,
                guides,
                is_last,
            } => {
                let start = lines.len();
                append_comment(
                    &mut lines, thread, *node, *depth, guides, *is_last, selected, width,
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

    EntryLayout { lines, comments }
}

#[allow(clippy::too_many_arguments)]
fn append_comment(
    lines: &mut Vec<Line<'static>>,
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
        format!("{marker} {tree}{branch} {author}")
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
                    for rich_line in render_rich_text(text) {
                        for mut wrapped in wrap_styled_line(rich_line, body_width) {
                            wrapped.spans.insert(0, Span::from(body_prefix.clone()));
                            lines.push(wrapped);
                        }
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

fn render_links(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let links = browser.current_links();
    let (selected, offset) = browser.link_position();
    let mut lines = Vec::new();

    if links.is_empty() {
        lines.push(Line::from(
            "No validated links are available in this context.",
        ));
    }
    for (index, link) in links
        .iter()
        .enumerate()
        .skip(offset)
        .take(page_len(size.height))
    {
        let marker = if index == selected { ">" } else { " " };
        lines.push(Line::from(Span::styled(
            format!("{marker} {}", link.label),
            selected_style(index == selected),
        )));
        lines.push(Line::from(format!("  {}", link.destination)));
    }

    let panel = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(format!(" HNSSH · {} ", browser.link_view_title()))
            .borders(Borders::ALL),
    );
    frame.render_widget(panel, body);
    frame.render_widget(
        Paragraph::new(
            "j/k move · o primary destination · Escape/Backspace back · ? help · q quit",
        )
        .alignment(Alignment::Center),
        footer,
    );
}

fn render_destination(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let mut lines = vec![
        Line::from("Use your terminal's native URL detection or copy the text below."),
        Line::from(""),
    ];
    if let Some(link) = browser.primary_destination() {
        lines.push(Line::from(Span::styled(
            link.label.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        lines.push(Line::from(link.destination.clone()));
    } else {
        lines.push(Line::from(
            "No primary external destination is available for this entry.",
        ));
    }
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .title(" HNSSH · Primary destination ")
                .borders(Borders::ALL),
        ),
        body,
    );
    frame.render_widget(
        Paragraph::new("Escape/Backspace back · ? help · q quit").alignment(Alignment::Center),
        footer,
    );
}

fn render_help(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let lines = vec![
        Line::from(Span::styled(
            "Keyboard help",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("Up / k                 Select previous visible row"),
        Line::from("Down / j               Select next visible row"),
        Line::from("Page Up / Page Down    Page through feed or rendered Entry content"),
        Line::from("g / G                  First / last feed row or Entry content"),
        Line::from("Enter                  Open an Entry; load or retry selected comments"),
        Line::from("Left / Right           Collapse or move to parent / expand a comment"),
        Line::from("Space                  Toggle the selected comment subtree"),
        Line::from("n / p                  Next / previous sibling comment"),
        Line::from("P / N                  Parent / first child comment"),
        Line::from("C / E                  Collapse / expand the whole Comment thread"),
        Line::from("o                      Expose the primary external destination"),
        Line::from("l                      Open contextual validated links"),
        Line::from("Escape / Backspace     Return to the previous view"),
        Line::from("?                      Open this help"),
        Line::from("r                      Refresh the feed or selected comment"),
        Line::from("q                      Quit HNSSH"),
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

fn entry_lines(entry: &FeedEntry) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        entry.title().to_owned(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    let mut context = vec![entry.kind_label().to_owned()];
    if let Some(details) = entry.details() {
        append_compact_metadata(&mut context, details);
    }
    lines.push(Line::from(context.join(" · ")));
    if let Some(details) = entry.details() {
        if let Some(published_at) = details.published_at {
            lines.push(Line::from(format!(
                "Published: {}",
                absolute_utc(published_at)
            )));
        }
        if let Some(text) = &details.text {
            lines.push(Line::from(""));
            lines.extend(render_rich_text(text));
        }
        if details.omitted_link_count > 0 {
            lines.push(Line::from(""));
            lines.push(Line::from(format!(
                "Unsafe or malformed links omitted: {}",
                details.omitted_link_count
            )));
        }
    }
    if let FeedEntry::Poll { options, .. } = entry {
        lines.push(Line::from(""));
        lines.push(Line::from("Poll options:"));
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
            lines.push(Line::from(format!("• {}{score}{status}", option.text)));
        }
    }
    lines
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

fn physical_lines(lines: Vec<Line<'static>>, width: usize) -> Vec<Line<'static>> {
    lines
        .into_iter()
        .flat_map(|line| wrap_styled_line(line, width.max(1)))
        .collect()
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

fn render_rich_text(text: &RichText) -> Vec<Line<'static>> {
    text.lines
        .iter()
        .map(|line| {
            let mut spans = Vec::new();
            if line.preformatted {
                spans.push(Span::from("│ "));
            }
            spans.extend(line.spans.iter().map(|span| {
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
                if span.style.link {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                Span::styled(span.text.clone(), style)
            }));
            Line::from(spans)
        })
        .collect()
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
