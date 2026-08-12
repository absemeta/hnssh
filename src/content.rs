use std::future::Future;
use std::pin::Pin;
use std::time::{Duration, SystemTime};

use thiserror::Error;

pub type FeedEntryId = u64;

pub(crate) type ContentFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The browsing boundary for Hacker News content.
///
/// Browsing sessions depend on this interface rather than on fixture or HTTP
/// details. The live adapter can therefore replace [`FixtureContent`] without
/// changing navigation state.
pub trait ContentSource: Send + Sync {
    fn top_feed(&self) -> ContentFuture<'_, Result<Vec<FeedEntryId>, ContentError>>;

    fn feed_entry(&self, id: FeedEntryId) -> ContentFuture<'_, Result<FeedEntry, ContentError>>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ContentError {
    #[error("content is unavailable")]
    Unavailable,
    #[error("content is temporarily unavailable")]
    Retryable,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EntryDetails {
    pub id: FeedEntryId,
    pub title: Option<String>,
    pub source: Option<String>,
    pub score: Option<u32>,
    pub author: Option<String>,
    pub published_at: Option<SystemTime>,
    pub comment_count: Option<u32>,
    pub text: Option<String>,
    pub links: Vec<Link>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PollOption {
    pub text: String,
    pub score: Option<u32>,
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
                text: Some("Build calm tools for noisy systems.".to_owned()),
                links: vec![Link::new(
                    "Acme careers",
                    "https://jobs.example.com/terminal-engineer",
                )],
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
                    text: None,
                    links: Vec::new(),
                },
                options: vec![
                    PollOption {
                        text: "Escape".to_owned(),
                        score: Some(31),
                    },
                    PollOption {
                        text: "Control".to_owned(),
                        score: Some(24),
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
    fn top_feed(&self) -> ContentFuture<'_, Result<Vec<FeedEntryId>, ContentError>> {
        Box::pin(async move { Ok(self.entries.iter().map(FeedEntry::id).collect()) })
    }

    fn feed_entry(&self, id: FeedEntryId) -> ContentFuture<'_, Result<FeedEntry, ContentError>> {
        Box::pin(async move {
            self.entries
                .iter()
                .find(|entry| entry.id() == id)
                .cloned()
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
        text: text.map(str::to_owned),
        links,
    })
}

fn ago(now: SystemTime, duration: Duration) -> Option<SystemTime> {
    now.checked_sub(duration)
}
