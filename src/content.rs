use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, Instant, SystemTime};

use thiserror::Error;

pub type FeedEntryId = u64;
pub type CommentId = FeedEntryId;

pub(crate) type ContentFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The browsing boundary for Hacker News content.
///
/// Browsing sessions depend on this interface rather than on fixture or HTTP
/// details. The live adapter can therefore replace [`FixtureContent`] without
/// changing navigation state.
pub trait ContentSource: Send + Sync {
    fn top_feed(
        &self,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>>;

    fn refresh_top_feed(
        &self,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>> {
        self.top_feed()
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<FeedEntry>, ContentError>>;

    fn refresh_feed_entry(
        &self,
        id: FeedEntryId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<FeedEntry>, ContentError>> {
        self.feed_entry(id)
    }

    fn comment(
        &self,
        _id: CommentId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Comment>, ContentError>> {
        Box::pin(async { Err(ContentError::Unavailable) })
    }

    fn refresh_comment(
        &self,
        id: CommentId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Comment>, ContentError>> {
        self.comment(id)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ContentError {
    #[error("content is unavailable")]
    Unavailable,
    #[error("content is temporarily unavailable")]
    Retryable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentFreshness {
    Fresh,
    Stale { age: Duration },
}

impl ContentFreshness {
    pub fn is_fresh(self) -> bool {
        matches!(self, Self::Fresh)
    }

    pub fn is_stale(self) -> bool {
        matches!(self, Self::Stale { .. })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContentSnapshot<T> {
    pub value: T,
    pub freshness: ContentFreshness,
    cached_at: Instant,
    stale_until: Option<Instant>,
}

impl<T> ContentSnapshot<T> {
    pub fn fresh(value: T) -> Self {
        Self {
            value,
            freshness: ContentFreshness::Fresh,
            cached_at: Instant::now(),
            stale_until: None,
        }
    }

    pub fn current_age(&self) -> Duration {
        Instant::now().saturating_duration_since(self.cached_at)
    }

    pub(crate) fn cached(
        value: T,
        freshness: ContentFreshness,
        age: Duration,
        stale_for: Duration,
    ) -> Self {
        let now = Instant::now();
        let cached_at = now.checked_sub(age).unwrap_or(now);
        Self {
            value,
            freshness,
            cached_at,
            stale_until: cached_at.checked_add(stale_for),
        }
    }

    pub(crate) fn mark_stale(&mut self) {
        self.freshness = ContentFreshness::Stale {
            age: self.current_age(),
        };
    }

    pub(crate) fn usable_after_failure(&self) -> bool {
        self.stale_until
            .is_none_or(|stale_until| Instant::now() <= stale_until)
    }

    pub(crate) fn metadata_only(&self) -> ContentSnapshot<()> {
        ContentSnapshot {
            value: (),
            freshness: self.freshness,
            cached_at: self.cached_at,
            stale_until: self.stale_until,
        }
    }

    pub(crate) fn try_map<U, E>(
        self,
        map: impl FnOnce(T) -> Result<U, E>,
    ) -> Result<ContentSnapshot<U>, E> {
        Ok(ContentSnapshot {
            value: map(self.value)?,
            freshness: self.freshness,
            cached_at: self.cached_at,
            stale_until: self.stale_until,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Link {
    pub label: String,
    pub destination: String,
}

impl Link {
    pub(crate) fn new(label: impl Into<String>, destination: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            destination: destination.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RichTextStyle {
    pub emphasis: bool,
    pub strong: bool,
    pub code: bool,
    pub link: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RichTextSpan {
    pub text: String,
    pub style: RichTextStyle,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RichTextLine {
    pub spans: Vec<RichTextSpan>,
    pub preformatted: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RichText {
    pub lines: Vec<RichTextLine>,
}

impl RichText {
    pub(crate) fn plain(value: impl Into<String>) -> Self {
        Self {
            lines: vec![RichTextLine {
                spans: vec![RichTextSpan {
                    text: value.into(),
                    style: RichTextStyle::default(),
                }],
                preformatted: false,
            }],
        }
    }

    pub fn plain_text(&self) -> String {
        self.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryDetails {
    pub id: FeedEntryId,
    pub title: Option<String>,
    pub source: Option<String>,
    pub score: Option<u32>,
    pub author: Option<String>,
    pub published_at: Option<SystemTime>,
    pub comment_count: Option<u32>,
    pub comment_ids: Vec<CommentId>,
    pub text: Option<RichText>,
    pub links: Vec<Link>,
    pub omitted_link_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommentDetails {
    pub id: CommentId,
    pub parent: Option<FeedEntryId>,
    pub author: Option<String>,
    pub published_at: Option<SystemTime>,
    pub text: Option<RichText>,
    pub child_ids: Vec<CommentId>,
    pub links: Vec<Link>,
    pub omitted_link_count: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Comment {
    Available(CommentDetails),
    Deleted {
        id: CommentId,
        child_ids: Vec<CommentId>,
    },
    Dead {
        id: CommentId,
        child_ids: Vec<CommentId>,
    },
    Missing {
        id: CommentId,
    },
    Unsupported {
        id: CommentId,
        item_type: String,
        child_ids: Vec<CommentId>,
    },
}

impl Comment {
    pub fn id(&self) -> CommentId {
        match self {
            Self::Available(details) => details.id,
            Self::Deleted { id, .. }
            | Self::Dead { id, .. }
            | Self::Missing { id }
            | Self::Unsupported { id, .. } => *id,
        }
    }

    pub fn child_ids(&self) -> &[CommentId] {
        match self {
            Self::Available(details) => &details.child_ids,
            Self::Deleted { child_ids, .. }
            | Self::Dead { child_ids, .. }
            | Self::Unsupported { child_ids, .. } => child_ids,
            Self::Missing { .. } => &[],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollOptionStatus {
    Available,
    Stale { age: Duration },
    Unavailable,
    Retryable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollOption {
    pub text: String,
    pub score: Option<u32>,
    pub status: PollOptionStatus,
}

impl PollOption {
    pub(crate) fn placeholder(status: PollOptionStatus) -> Self {
        let text = match status {
            PollOptionStatus::Unavailable => "[poll option unavailable]",
            PollOptionStatus::Retryable => "[poll option temporarily unavailable]",
            PollOptionStatus::Available | PollOptionStatus::Stale { .. } => {
                unreachable!("available poll options are not placeholders")
            }
        };
        Self {
            text: text.to_owned(),
            score: None,
            status,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FeedEntry {
    Story(EntryDetails),
    Job(EntryDetails),
    Poll {
        details: EntryDetails,
        options: Vec<PollOption>,
    },
    Unavailable {
        id: FeedEntryId,
    },
    Unsupported {
        id: FeedEntryId,
        item_type: String,
        title: Option<String>,
    },
}

impl FeedEntry {
    pub fn id(&self) -> FeedEntryId {
        match self {
            Self::Story(details) | Self::Job(details) => details.id,
            Self::Poll { details, .. } => details.id,
            Self::Unavailable { id } | Self::Unsupported { id, .. } => *id,
        }
    }

    pub fn details(&self) -> Option<&EntryDetails> {
        match self {
            Self::Story(details) | Self::Job(details) => Some(details),
            Self::Poll { details, .. } => Some(details),
            Self::Unavailable { .. } | Self::Unsupported { .. } => None,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Story(details) | Self::Job(details) | Self::Poll { details, .. } => {
                details.title.as_deref().unwrap_or("[title unavailable]")
            }
            Self::Unavailable { .. } => "[feed entry unavailable]",
            Self::Unsupported { title, .. } => {
                title.as_deref().unwrap_or("[unsupported feed entry]")
            }
        }
    }

    pub fn kind_label(&self) -> &str {
        match self {
            Self::Story(_) => "Story",
            Self::Job(_) => "Job",
            Self::Poll { .. } => "Poll",
            Self::Unavailable { .. } => "Unavailable",
            Self::Unsupported { .. } => "Unsupported item",
        }
    }

    pub fn links(&self) -> &[Link] {
        self.details()
            .map_or(&[], |details| details.links.as_slice())
    }
}

/// Deterministic local content used until the live Hacker News adapter lands.
#[derive(Clone, Debug)]
pub struct FixtureContent {
    entries: Vec<FeedEntry>,
}

impl Default for FixtureContent {
    fn default() -> Self {
        Self::at(SystemTime::now())
    }
}

impl FixtureContent {
    pub fn at(now: SystemTime) -> Self {
        let entries = vec![
            story(
                101,
                "Rust reaches the tiny terminal",
                story_metadata(
                    Some("example.com"),
                    342,
                    "ada",
                    ago(now, Duration::from_secs(2 * 60 * 60)),
                    88,
                ),
                Some("A small terminal browser can still have a deep interface."),
                vec![Link::new(
                    "Rust reaches the tiny terminal",
                    "https://example.com/tiny-terminal",
                )],
            ),
            story(
                102,
                "Ask HN: What are you building this week?",
                story_metadata(
                    None,
                    195,
                    "linus",
                    ago(now, Duration::from_secs(60 * 60)),
                    241,
                ),
                Some("A fixture discussion for trying the detail view."),
                vec![Link::new(
                    "The Rust Programming Language",
                    "https://doc.rust-lang.org/book/",
                )],
            ),
            FeedEntry::Job(EntryDetails {
                id: 103,
                title: Some("Acme is hiring terminal-minded engineers".to_owned()),
                source: Some("jobs.example.com".to_owned()),
                score: None,
                author: None,
                published_at: ago(now, Duration::from_secs(3 * 60 * 60)),
                comment_count: None,
                comment_ids: Vec::new(),
                text: Some(RichText::plain("Build calm tools for noisy systems.")),
                links: vec![Link::new(
                    "Acme careers",
                    "https://jobs.example.com/terminal-engineer",
                )],
                omitted_link_count: 0,
            }),
            FeedEntry::Poll {
                details: EntryDetails {
                    id: 104,
                    title: Some("Which terminal key do you use most?".to_owned()),
                    source: None,
                    score: Some(73),
                    author: Some("grace".to_owned()),
                    published_at: ago(now, Duration::from_secs(4 * 60 * 60)),
                    comment_count: Some(39),
                    comment_ids: Vec::new(),
                    text: None,
                    links: Vec::new(),
                    omitted_link_count: 0,
                },
                options: vec![
                    PollOption {
                        text: "Escape".to_owned(),
                        score: Some(31),
                        status: PollOptionStatus::Available,
                    },
                    PollOption {
                        text: "Control".to_owned(),
                        score: Some(24),
                        status: PollOptionStatus::Available,
                    },
                ],
            },
            story(
                105,
                "A tour of async Rust",
                story_metadata(
                    Some("rust-lang.org"),
                    128,
                    "ferris",
                    ago(now, Duration::from_secs(5 * 60 * 60)),
                    42,
                ),
                None,
                vec![Link::new(
                    "A tour of async Rust",
                    "https://rust-lang.org/async-book/",
                )],
            ),
            story(
                106,
                "Designing software around deep modules",
                story_metadata(
                    Some("example.net"),
                    117,
                    "parnas",
                    ago(now, Duration::from_secs(6 * 60 * 60)),
                    35,
                ),
                None,
                Vec::new(),
            ),
            story(
                107,
                "Why text interfaces endure",
                story_metadata(
                    Some("example.org"),
                    96,
                    "kay",
                    ago(now, Duration::from_secs(7 * 60 * 60)),
                    27,
                ),
                None,
                Vec::new(),
            ),
            story(
                108,
                "Show HN: A pocket-sized parser",
                story_metadata(
                    Some("parser.example"),
                    84,
                    "ritchie",
                    ago(now, Duration::from_secs(8 * 60 * 60)),
                    19,
                ),
                None,
                Vec::new(),
            ),
            story(
                109,
                "Making failure states useful",
                story_metadata(
                    Some("reliability.example"),
                    76,
                    "hamilton",
                    ago(now, Duration::from_secs(9 * 60 * 60)),
                    14,
                ),
                None,
                Vec::new(),
            ),
            story(
                110,
                "The surprising cost of an unbounded queue",
                story_metadata(
                    Some("systems.example"),
                    69,
                    "lamport",
                    ago(now, Duration::from_secs(10 * 60 * 60)),
                    22,
                ),
                None,
                Vec::new(),
            ),
            FeedEntry::Unavailable { id: 111 },
            story(
                112,
                "The art of readable terminals",
                story_metadata(
                    Some("terminal.example"),
                    61,
                    "lovelace",
                    ago(now, Duration::from_secs(12 * 60 * 60)),
                    11,
                ),
                None,
                Vec::new(),
            ),
        ];

        Self { entries }
    }
}

impl ContentSource for FixtureContent {
    fn top_feed(
        &self,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>> {
        Box::pin(async move {
            Ok(ContentSnapshot::fresh(
                self.entries.iter().map(FeedEntry::id).collect(),
            ))
        })
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<FeedEntry>, ContentError>> {
        Box::pin(async move {
            self.entries
                .iter()
                .find(|entry| entry.id() == id)
                .cloned()
                .map(ContentSnapshot::fresh)
                .ok_or(ContentError::Unavailable)
        })
    }
}

struct StoryMetadata<'a> {
    source: Option<&'a str>,
    score: u32,
    author: &'a str,
    published_at: Option<SystemTime>,
    comment_count: u32,
}

fn story_metadata<'a>(
    source: Option<&'a str>,
    score: u32,
    author: &'a str,
    published_at: Option<SystemTime>,
    comment_count: u32,
) -> StoryMetadata<'a> {
    StoryMetadata {
        source,
        score,
        author,
        published_at,
        comment_count,
    }
}

fn story(
    id: FeedEntryId,
    title: &str,
    metadata: StoryMetadata<'_>,
    text: Option<&str>,
    links: Vec<Link>,
) -> FeedEntry {
    FeedEntry::Story(EntryDetails {
        id,
        title: Some(title.to_owned()),
        source: metadata.source.map(str::to_owned),
        score: Some(metadata.score),
        author: Some(metadata.author.to_owned()),
        published_at: metadata.published_at,
        comment_count: Some(metadata.comment_count),
        comment_ids: Vec::new(),
        text: text.map(RichText::plain),
        links,
        omitted_link_count: 0,
    })
}

fn ago(now: SystemTime, duration: Duration) -> Option<SystemTime> {
    now.checked_sub(duration)
}
