use std::time::{Duration, SystemTime};

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::browser::{Browser, FeedRowState, FeedStatus, ViewKind};
use crate::content::{EntryDetails, FeedEntry};

pub(crate) const MIN_COLUMNS: u16 = 60;
pub(crate) const MIN_ROWS: u16 = 16;

pub(crate) fn page_len(rows: u16) -> usize {
    usize::from(rows.saturating_sub(3) / 2).max(1)
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
    let block = Block::default()
        .title(" HNSSH · Top feed ")
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
            lines.push(Line::from("Reconnect later to retry."));
        }
        FeedStatus::Ready if browser.rows().is_empty() => {
            lines.push(Line::from("The top feed is empty."));
        }
        FeedStatus::Ready => {
            let (selected, offset) = browser.feed_position();
            for (index, row) in browser
                .rows()
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
                    format!("{marker} {}. {}", row.rank, row.title()),
                    style,
                )));
                lines.push(Line::from(format!("    {}", feed_metadata(&row.state))));
            }
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
    frame.render_widget(
        Paragraph::new("↑/↓ or j/k move · PgUp/PgDn page · Enter details · ? help · q quit")
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

    let mut lines = vec![
        Line::from(Span::styled(
            entry.title(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(format!("Type: {}", entry.kind_label())),
    ];
    if let Some(details) = entry.details() {
        append_detail_metadata(&mut lines, details);
        if let Some(text) = &details.text {
            lines.push(Line::from(""));
            lines.push(Line::from(text.as_str()));
        }
    }
    if let FeedEntry::Poll { options, .. } = entry {
        lines.push(Line::from(""));
        lines.push(Line::from("Poll options:"));
        for option in options {
            let score = option
                .score
                .map(|score| format!(" — {score} points"))
                .unwrap_or_default();
            lines.push(Line::from(format!("• {}{score}", option.text)));
        }
    }
    lines.push(Line::from(""));
    if entry.links().is_empty() {
        lines.push(Line::from("No links available"));
    } else {
        lines.push(Line::from(Span::styled(
            "Enter  View links",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )));
    }

    let panel = Paragraph::new(lines)
        .scroll((browser.current_scroll(), 0))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .title(" HNSSH · Entry details ")
                .borders(Borders::ALL),
        );
    frame.render_widget(panel, body);
    frame.render_widget(
        Paragraph::new("↑/↓ scroll · Enter links · Escape/Backspace back · ? help · q quit")
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

    let panel = Paragraph::new(lines).block(
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
        Line::from("Enter                  Open the selected entry or its links"),
        Line::from("Escape / Backspace     Return to the previous view"),
        Line::from("?                      Open this help"),
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
        FeedRowState::Loading => "waiting for fixture content".to_owned(),
        FeedRowState::Unavailable => "unavailable".to_owned(),
        FeedRowState::Ready(entry) => entry
            .details()
            .map(metadata)
            .filter(|metadata| !metadata.is_empty())
            .unwrap_or_else(|| entry.kind_label().to_owned()),
    }
}

fn append_detail_metadata(lines: &mut Vec<Line<'_>>, details: &EntryDetails) {
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
    }
    if let Some(comment_count) = details.comment_count {
        lines.push(Line::from(format!("Comments: {comment_count}")));
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

fn quantity(value: u64, unit: &str) -> String {
    let suffix = if value == 1 { "" } else { "s" };
    format!("{value} {unit}{suffix} ago")
}
