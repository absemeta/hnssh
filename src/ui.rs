use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::browser::{Browser, DetailAction, FeedRowState, FeedStatus, ViewKind};
use crate::content::{
    Comment, ContentFreshness, EntryDetails, FeedEntry, PollOptionStatus, RichText,
};
use crate::thread::{ThreadNodeState, ThreadRow};

pub(crate) const MIN_COLUMNS: u16 = 60;
pub(crate) const MIN_ROWS: u16 = 16;

pub(crate) fn page_len(rows: u16) -> usize {
    usize::from(rows.saturating_sub(3) / 2).max(1)
}

pub(crate) fn detail_scroll_max(browser: &Browser, size: Rect) -> u16 {
    let Some(entry) = browser.current_entry() else {
        return 0;
    };
    let width = usize::from(size.width.saturating_sub(2).max(1));
    let visible_rows = usize::from(size.height.saturating_sub(3).max(1));
    let content_rows = detail_lines(browser, entry)
        .iter()
        .map(|line| line.width().max(1).div_ceil(width))
        .sum::<usize>();
    u16::try_from(content_rows.saturating_sub(visible_rows)).unwrap_or(u16::MAX)
}

pub(crate) fn render(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    if (size.width < MIN_COLUMNS || size.height < MIN_ROWS) && browser.view_kind() != ViewKind::Help
    {
        render_resize(frame, size);
        return;
    }

    match browser.view_kind() {
        ViewKind::Feed => render_feed(frame, browser, size),
        ViewKind::Detail => render_detail(frame, browser, size),
        ViewKind::Thread => render_thread(frame, browser, size),
        ViewKind::Links => render_links(frame, browser, size),
        ViewKind::Help => render_help(frame, browser, size),
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
        FeedStatus::Loading => {
            lines.push(Line::from("Loading top feed…"));
        }
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
                let style = if index == selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
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
        Paragraph::new("j/k move · PgUp/PgDn page · Enter open/load · r refresh · ? help · q quit")
            .alignment(Alignment::Center),
        footer,
    );
}

fn render_detail(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let Some(entry) = browser.current_entry() else {
        frame.render_widget(Paragraph::new("Entry unavailable"), body);
        return;
    };

    let panel = Paragraph::new(detail_lines(browser, entry))
        .scroll((browser.current_scroll(), 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" HNSSH · Entry details ")
                .borders(Borders::ALL),
        );
    frame.render_widget(panel, body);
    frame.render_widget(
        Paragraph::new(
            "↑/↓ choose action · PgUp/PgDn scroll · Enter open · Escape back · ? help · q quit",
        )
        .alignment(Alignment::Center),
        footer,
    );
}

fn thread_indent(depth: usize) -> String {
    const MAX_VISIBLE_INDENT: usize = 12;
    if depth <= MAX_VISIBLE_INDENT {
        "  ".repeat(depth)
    } else {
        format!("{}…{depth} ", "  ".repeat(MAX_VISIBLE_INDENT))
    }
}

fn render_thread(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let Some(thread) = browser.current_thread() else {
        frame.render_widget(Paragraph::new("Comment thread unavailable"), body);
        return;
    };
    let rows = thread.rows();
    let (selected, offset) = thread.position();
    let mut lines = Vec::new();
    for (position, row) in rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(page_len(size.height))
    {
        let selected = position == selected;
        let marker = if selected { ">" } else { " " };
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        match row {
            ThreadRow::Comment { node, depth } => {
                let Some(comment) = thread.node(*node) else {
                    continue;
                };
                let indent = thread_indent(*depth);
                match comment.state() {
                    ThreadNodeState::Loading => lines.push(Line::from(Span::styled(
                        format!("{marker} {indent}[loading comment {}]", comment.id()),
                        style,
                    ))),
                    ThreadNodeState::Unavailable => lines.push(Line::from(Span::styled(
                        format!("{marker} {indent}[comment unavailable]"),
                        style,
                    ))),
                    ThreadNodeState::Retryable => lines.push(Line::from(Span::styled(
                        format!("{marker} {indent}[comment temporarily unavailable]"),
                        style,
                    ))),
                    ThreadNodeState::Ready(snapshot) => {
                        let branch = if comment.has_children() {
                            if comment.expanded() { "▼" } else { "▶" }
                        } else {
                            "•"
                        };
                        match &snapshot.value {
                            Comment::Available(details) => {
                                let author =
                                    details.author.as_deref().unwrap_or("[author unavailable]");
                                let age = details
                                    .published_at
                                    .map(relative_age)
                                    .map(|age| format!(" · {age}"))
                                    .unwrap_or_default();
                                let stale = if snapshot.freshness.is_stale() {
                                    format!(" · stale · {} old", cache_age(snapshot.current_age()))
                                } else {
                                    String::new()
                                };
                                let refreshing = if comment.refreshing() {
                                    " · refreshing…"
                                } else {
                                    ""
                                };
                                lines.push(Line::from(Span::styled(
                                    format!(
                                        "{marker} {indent}{branch} {author}{age}{stale}{refreshing}"
                                    ),
                                    style,
                                )));
                                if let Some(text) = &details.text {
                                    let text_indent = format!("  {indent}  ");
                                    for mut line in render_rich_text(text) {
                                        line.spans.insert(0, Span::from(text_indent.clone()));
                                        lines.push(line);
                                    }
                                } else {
                                    lines.push(Line::from(format!(
                                        "  {indent}  [comment text unavailable]"
                                    )));
                                }
                                if details.omitted_link_count > 0 {
                                    lines.push(Line::from(format!(
                                        "  {indent}  Unsafe or malformed links omitted: {}",
                                        details.omitted_link_count
                                    )));
                                }
                            }
                            Comment::Deleted { .. } => lines.push(Line::from(Span::styled(
                                format!("{marker} {indent}{branch} [deleted comment]"),
                                style,
                            ))),
                            Comment::Dead { .. } => lines.push(Line::from(Span::styled(
                                format!("{marker} {indent}{branch} [dead comment]"),
                                style,
                            ))),
                            Comment::Missing { .. } => lines.push(Line::from(Span::styled(
                                format!("{marker} {indent}• [comment missing]"),
                                style,
                            ))),
                            Comment::Unsupported { item_type, .. } => {
                                lines.push(Line::from(Span::styled(
                                    format!("{marker} {indent}{branch} [unsupported {item_type}]"),
                                    style,
                                )))
                            }
                        }
                    }
                }
            }
            ThreadRow::LoadMore {
                depth, remaining, ..
            } => {
                let indent = thread_indent(*depth);
                let count = (*remaining).min(20);
                lines.push(Line::from(Span::styled(
                    format!("{marker} {indent}Load {count} more comments"),
                    style,
                )));
            }
            ThreadRow::Marker { depth, label } => {
                let indent = thread_indent(*depth);
                lines.push(Line::from(format!("{marker} {indent}{label}")));
            }
        }
    }

    let panel = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" HNSSH · Comment thread ")
            .borders(Borders::ALL),
    );
    frame.render_widget(panel, body);
    frame.render_widget(
        Paragraph::new("j/k move · ← collapse/parent · → expand · Space toggle · Enter load/retry · ? help · q quit")
            .alignment(Alignment::Center),
        footer,
    );
}

fn render_links(frame: &mut Frame<'_>, browser: &Browser, size: Rect) {
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(size);
    let links = browser.current_links();
    let (selected, offset) = browser.link_position();
    let mut lines = Vec::new();

    for (index, link) in links
        .iter()
        .enumerate()
        .skip(offset)
        .take(page_len(size.height))
    {
        let marker = if index == selected { ">" } else { " " };
        let style = if index == selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(
            format!("{marker} {}", link.label),
            style,
        )));
        lines.push(Line::from(format!("  {}", link.destination)));
    }

    let panel = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .title(" HNSSH · Entry links ")
            .borders(Borders::ALL),
    );
    frame.render_widget(panel, body);
    frame.render_widget(
        Paragraph::new("↑/↓ or j/k move · Escape/Backspace back · ? help · q quit")
            .alignment(Alignment::Center),
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
        Line::from("Up / k                 Move up"),
        Line::from("Down / j               Move down"),
        Line::from("Page Up / Page Down    Move by one page"),
        Line::from("g / G                  First / last loaded row"),
        Line::from("Enter                  Open the selected entry or action; load/retry comments"),
        Line::from("Left / Right           Collapse or move to parent / expand a comment"),
        Line::from("Space                  Toggle the selected comment subtree"),
        Line::from("Escape / Backspace     Return to the previous view"),
        Line::from("?                      Open this help"),
        Line::from("r                      Refresh or retry the current feed or comment"),
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

fn detail_lines(browser: &Browser, entry: &FeedEntry) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            entry.title().to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Type: {}", entry.kind_label())),
    ];
    if let Some(details) = entry.details() {
        append_detail_metadata(&mut lines, details);
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
                PollOptionStatus::Stale { age } => {
                    format!(" · stale · {} old", cache_age(age))
                }
                PollOptionStatus::Unavailable | PollOptionStatus::Retryable => {
                    " · not loaded".to_owned()
                }
            };
            lines.push(Line::from(format!("• {}{score}{status}", option.text)));
        }
    }
    lines.push(Line::from(""));
    let actions = browser.detail_actions();
    if actions.is_empty() {
        lines.push(Line::from("No comments or links available"));
    } else {
        let selected = browser.detail_action_position();
        for (position, action) in actions.iter().enumerate() {
            let marker = if position == selected { ">" } else { " " };
            let label = match action {
                DetailAction::Comments => "View comments",
                DetailAction::Links => "View links",
            };
            let style = if position == selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(
                format!("{marker} Enter  {label}"),
                style,
            )));
        }
    }
    lines
}

fn append_detail_metadata(lines: &mut Vec<Line<'static>>, details: &EntryDetails) {
    if let Some(source) = &details.source {
        lines.push(Line::from(format!("Source: {source}")));
    }
    if let Some(score) = details.score {
        lines.push(Line::from(format!("Score: {}", points(score))));
    }
    if let Some(author) = &details.author {
        lines.push(Line::from(format!("Author: {author}")));
    }
    if let Some(published_at) = details.published_at {
        lines.push(Line::from(format!("Age: {}", relative_age(published_at))));
        lines.push(Line::from(format!(
            "Published: {}",
            absolute_utc(published_at)
        )));
    }
    if let Some(comment_count) = details.comment_count {
        lines.push(Line::from(format!("Comments: {comment_count}")));
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
