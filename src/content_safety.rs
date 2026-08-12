use reqwest::Url;
use scraper::{ElementRef, Html, Node};

use crate::content::{Link, RichText, RichTextLine, RichTextSpan, RichTextStyle};

pub(crate) struct NormalizedContent {
    pub(crate) text: RichText,
    pub(crate) links: Vec<Link>,
    pub(crate) omitted_link_count: usize,
}

pub(crate) struct ValidatedUrl {
    pub(crate) destination: String,
    pub(crate) source: Option<String>,
}

pub(crate) fn normalize_html(value: &str) -> NormalizedContent {
    let fragment = Html::parse_fragment(value);
    let mut builder = RichTextBuilder::default();
    let mut links = Vec::new();
    let mut omitted_link_count = 0;
    walk_children(
        fragment.root_element(),
        RichTextStyle::default(),
        false,
        &mut builder,
        &mut links,
        &mut omitted_link_count,
        true,
    );
    NormalizedContent {
        text: builder.finish(),
        links,
        omitted_link_count,
    }
}

pub(crate) fn normalize_inline_text(value: &str) -> Option<String> {
    collapse_to_line(normalize_html(value).text)
}

pub(crate) fn validated_url(value: &str) -> Option<ValidatedUrl> {
    if value.chars().any(forbidden_url_character) {
        return None;
    }
    let value = value.trim();
    let url = Url::parse(value).ok()?;
    if !matches!(url.scheme(), "http" | "https")
        || !url.has_host()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let source = url.host_str().map(str::to_owned);
    Some(ValidatedUrl {
        destination: url.to_string(),
        source,
    })
}

fn forbidden_url_character(character: char) -> bool {
    character.is_control() || is_bidi_control(character)
}

fn walk_children<'a>(
    element: ElementRef<'a>,
    style: RichTextStyle,
    preformatted: bool,
    builder: &mut RichTextBuilder,
    links: &mut Vec<Link>,
    omitted_link_count: &mut usize,
    collect_links: bool,
) {
    let mut actions = Vec::new();
    push_children(&mut actions, element, style, preformatted);

    while let Some(action) = actions.pop() {
        match action {
            WalkAction::Text(value, style, preformatted) => {
                builder.push_text(value, style, preformatted);
            }
            WalkAction::SeparateBlock => builder.separate_block(),
            WalkAction::LineBreak => builder.line_break(),
            WalkAction::Element(element, mut style, preformatted) => {
                match element.value().name() {
                    "script" | "style" => continue,
                    "br" => {
                        actions.push(WalkAction::LineBreak);
                        continue;
                    }
                    "p" | "div" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        builder.separate_block();
                        actions.push(WalkAction::SeparateBlock);
                    }
                    "li" => {
                        builder.separate_block();
                        builder.push_text("• ", style, false);
                        actions.push(WalkAction::SeparateBlock);
                    }
                    "pre" => {
                        builder.separate_block();
                        style.code = true;
                        actions.push(WalkAction::SeparateBlock);
                        push_children(&mut actions, element, style, true);
                        continue;
                    }
                    "em" | "i" => style.emphasis = true,
                    "strong" | "b" => style.strong = true,
                    "code" => style.code = true,
                    "a" if collect_links => {
                        if let Some(destination) = element.value().attr("href") {
                            if let Some(destination) = validated_url(destination) {
                                let label = normalized_link_label(element)
                                    .unwrap_or_else(|| destination.destination.clone());
                                links.push(Link::new(label, destination.destination));
                                style.link = true;
                            } else {
                                *omitted_link_count = omitted_link_count.saturating_add(1);
                            }
                        }
                    }
                    _ => {}
                }
                push_children(&mut actions, element, style, preformatted);
            }
        }
    }
}

#[derive(Clone, Copy)]
enum WalkAction<'a> {
    Element(ElementRef<'a>, RichTextStyle, bool),
    Text(&'a str, RichTextStyle, bool),
    SeparateBlock,
    LineBreak,
}

fn push_children<'a>(
    actions: &mut Vec<WalkAction<'a>>,
    element: ElementRef<'a>,
    style: RichTextStyle,
    preformatted: bool,
) {
    for child in element.children().rev() {
        match child.value() {
            Node::Text(text) => actions.push(WalkAction::Text(text, style, preformatted)),
            Node::Element(_) => {
                if let Some(child) = ElementRef::wrap(child) {
                    actions.push(WalkAction::Element(child, style, preformatted));
                }
            }
            Node::Document
            | Node::Fragment
            | Node::Doctype(_)
            | Node::Comment(_)
            | Node::ProcessingInstruction(_) => {}
        }
    }
}

fn normalized_link_label(element: ElementRef<'_>) -> Option<String> {
    let mut builder = RichTextBuilder::default();
    let mut ignored_links = Vec::new();
    let mut ignored_omissions = 0;
    walk_children(
        element,
        RichTextStyle::default(),
        false,
        &mut builder,
        &mut ignored_links,
        &mut ignored_omissions,
        false,
    );
    collapse_to_line(builder.finish())
}

fn collapse_to_line(text: RichText) -> Option<String> {
    let line = text
        .plain_text()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!line.is_empty()).then_some(line)
}

#[derive(Default)]
struct RichTextBuilder {
    lines: Vec<RichTextLine>,
    current_spans: Vec<RichTextSpan>,
    current_preformatted: bool,
    pending_space: bool,
}

impl RichTextBuilder {
    fn push_text(&mut self, value: &str, style: RichTextStyle, preformatted: bool) {
        let value = sanitize_terminal_text(value, preformatted);
        if preformatted {
            self.push_preformatted(&value, style);
        } else {
            self.push_collapsed(&value, style);
        }
    }

    fn push_collapsed(&mut self, value: &str, style: RichTextStyle) {
        for character in value.chars() {
            if character.is_whitespace() {
                self.pending_space = true;
                continue;
            }
            if self.pending_space && !self.current_spans.is_empty() {
                self.push_character(' ', style);
            }
            self.pending_space = false;
            self.push_character(character, style);
        }
    }

    fn push_preformatted(&mut self, value: &str, style: RichTextStyle) {
        self.pending_space = false;
        self.current_preformatted = true;
        for character in value.chars() {
            if character == '\n' {
                self.finish_line(true);
                self.current_preformatted = true;
            } else {
                self.push_character(character, style);
            }
        }
    }

    fn push_character(&mut self, character: char, style: RichTextStyle) {
        if let Some(span) = self
            .current_spans
            .last_mut()
            .filter(|span| span.style == style)
        {
            span.text.push(character);
        } else {
            self.current_spans.push(RichTextSpan {
                text: character.to_string(),
                style,
            });
        }
    }

    fn line_break(&mut self) {
        self.pending_space = false;
        self.finish_line(true);
    }

    fn separate_block(&mut self) {
        self.pending_space = false;
        if !self.current_spans.is_empty() {
            self.finish_line(false);
        }
        if self.lines.last().is_some_and(|line| !line.spans.is_empty()) {
            self.lines.push(RichTextLine {
                spans: Vec::new(),
                preformatted: false,
            });
        }
    }

    fn finish_line(&mut self, include_empty: bool) {
        if include_empty || !self.current_spans.is_empty() {
            self.lines.push(RichTextLine {
                spans: std::mem::take(&mut self.current_spans),
                preformatted: self.current_preformatted,
            });
        }
        self.current_spans.clear();
        self.current_preformatted = false;
    }

    fn finish(mut self) -> RichText {
        self.pending_space = false;
        if !self.current_spans.is_empty() {
            self.finish_line(false);
        }
        while self.lines.first().is_some_and(|line| line.spans.is_empty()) {
            self.lines.remove(0);
        }
        while self.lines.last().is_some_and(|line| line.spans.is_empty()) {
            self.lines.pop();
        }
        RichText { lines: self.lines }
    }
}

fn sanitize_terminal_text(value: &str, preserve_line_breaks: bool) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    let mut sanitized = String::with_capacity(value.len());
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        if character == '\u{001b}' {
            index = skip_escape_sequence(&characters, index + 1);
            continue;
        }
        if character == '\u{009b}' {
            index = skip_control_sequence(&characters, index + 1);
            continue;
        }
        if character == '\r' {
            if preserve_line_breaks {
                sanitized.push('\n');
                if characters.get(index + 1) == Some(&'\n') {
                    index += 1;
                }
            } else {
                sanitized.push(' ');
            }
        } else if character == '\n' {
            sanitized.push(if preserve_line_breaks { '\n' } else { ' ' });
        } else if character == '\t' {
            if preserve_line_breaks {
                sanitized.push_str("    ");
            } else {
                sanitized.push(' ');
            }
        } else if !character.is_control() && !is_bidi_control(character) {
            sanitized.push(character);
        }
        index += 1;
    }

    sanitized
}

fn skip_escape_sequence(characters: &[char], mut index: usize) -> usize {
    match characters.get(index) {
        Some('[') => skip_control_sequence(characters, index + 1),
        Some(']') => {
            index += 1;
            while index < characters.len() {
                if characters[index] == '\u{0007}' {
                    return index + 1;
                }
                if characters[index] == '\u{001b}' && characters.get(index + 1) == Some(&'\\') {
                    return index + 2;
                }
                index += 1;
            }
            index
        }
        Some(_) => index + 1,
        None => index,
    }
}

fn skip_control_sequence(characters: &[char], mut index: usize) -> usize {
    while index < characters.len() {
        let character = characters[index];
        index += 1;
        if ('@'..='~').contains(&character) {
            break;
        }
    }
    index
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_markup_and_rejects_active_or_credentialed_links() {
        let normalized = normalize_html(
            "<p>Hello <em>world</em> &amp; <code>x</code>.</p>\
             <script>hidden</script><p><a href='https://example.com/?a=1&amp;b=2'>safe</a> \
             <a href='javascript:alert(1)'>unsafe</a> \
             <a href='https://name:secret@example.com/'>credentialed</a></p>",
        );

        assert_eq!(
            normalized.text.plain_text(),
            "Hello world & x.\n\nsafe unsafe credentialed"
        );
        let spans = normalized
            .text
            .lines
            .iter()
            .flat_map(|line| &line.spans)
            .collect::<Vec<_>>();
        assert!(
            spans
                .iter()
                .any(|span| span.text.contains("world") && span.style.emphasis)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text.contains('x') && span.style.code)
        );
        assert!(
            spans
                .iter()
                .any(|span| span.text == "safe" && span.style.link)
        );
        assert_eq!(normalized.links.len(), 1);
        assert_eq!(normalized.omitted_link_count, 2);
        assert_eq!(
            normalized.links[0].destination,
            "https://example.com/?a=1&b=2"
        );
    }

    #[test]
    fn strips_terminal_sequences_and_preserves_preformatted_lines() {
        let normalized = normalize_html("<pre>a\t&lt;b&gt;\n\u{1b}[31mred\u{1b}[0m</pre>");

        assert_eq!(normalized.text.plain_text(), "a    <b>\nred");
        assert!(normalized.text.lines.iter().all(|line| line.preformatted));
    }

    #[test]
    fn tolerates_long_unicode_and_malformed_entities() {
        let input = format!("<p>{} &bogus; &#x110000; 🦀</p>", "é".repeat(4_096));
        let normalized = normalize_html(&input).text.plain_text();

        assert!(normalized.starts_with(&"é".repeat(4_096)));
        assert!(normalized.contains("🦀"));
        assert!(!normalized.chars().any(is_bidi_control));
    }
}
