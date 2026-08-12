use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::{Client, StatusCode, Url};
use serde::Deserialize;
use tokio::sync::{Mutex, oneshot};

use crate::content::{
    ContentError, ContentFuture, ContentSource, EntryDetails, FeedEntry, FeedEntryId,
};

const PRODUCTION_ENDPOINT: &str = "https://hacker-news.firebaseio.com/v0";

/// Official Hacker News v0 content backed by one process-wide coalescing cache.
///
/// Production construction fixes the endpoint to Hacker News. Tests may use
/// [`HackerNewsContent::for_endpoint`] to exercise the same adapter against a
/// controllable local HTTP service.
#[derive(Clone)]
pub struct HackerNewsContent {
    client: Client,
    endpoint: String,
    top_feed: Arc<SharedCache<(), Vec<FeedEntryId>>>,
    items: Arc<SharedCache<FeedEntryId, FeedEntry>>,
}

impl HackerNewsContent {
    pub fn production() -> Self {
        Self::for_endpoint(PRODUCTION_ENDPOINT)
    }

    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self {
            client: Client::new(),
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            top_feed: Arc::new(SharedCache::default()),
            items: Arc::new(SharedCache::default()),
        }
    }
}

impl ContentSource for HackerNewsContent {
    fn top_feed(&self) -> ContentFuture<'_, Result<Vec<FeedEntryId>, ContentError>> {
        Box::pin(async move {
            let client = self.client.clone();
            let url = format!("{}/topstories.json", self.endpoint);
            self.top_feed
                .get_or_fetch((), async move { fetch_json(&client, &url).await })
                .await
        })
    }

    fn feed_entry(&self, id: FeedEntryId) -> ContentFuture<'_, Result<FeedEntry, ContentError>> {
        Box::pin(async move {
            let client = self.client.clone();
            let url = format!("{}/item/{id}.json", self.endpoint);
            self.items
                .get_or_fetch(id, async move {
                    let item: Option<UpstreamItem> = fetch_json(&client, &url).await?;
                    item.map_or(Ok(FeedEntry::Unavailable { id }), |item| {
                        item.into_feed_entry(id)
                    })
                })
                .await
        })
    }
}

#[derive(Debug, Deserialize)]
struct UpstreamItem {
    id: Option<FeedEntryId>,
    #[serde(rename = "type")]
    item_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    by: Option<String>,
    time: Option<u64>,
    dead: Option<bool>,
    deleted: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    url: Option<String>,
    score: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    title: Option<String>,
    descendants: Option<u32>,
}

impl UpstreamItem {
    fn into_feed_entry(self, requested_id: FeedEntryId) -> Result<FeedEntry, ContentError> {
        if self.id.is_some_and(|id| id != requested_id) {
            return Err(ContentError::Unavailable);
        }
        if self.dead == Some(true) || self.deleted == Some(true) {
            return Ok(FeedEntry::Unavailable { id: requested_id });
        }

        let Some(item_type) = self.item_type else {
            return Ok(FeedEntry::Unavailable { id: requested_id });
        };
        let details = EntryDetails {
            id: requested_id,
            source: self.url.as_deref().and_then(url_source),
            // Rich text and links remain hidden until the content-safety slice
            // validates and normalizes them for terminal output.
            links: Vec::new(),
            title: self.title.map(|title| terminal_safe_text(&title)),
            score: self.score,
            author: self.by.map(|author| terminal_safe_text(&author)),
            published_at: self.time.and_then(|seconds| {
                SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds))
            }),
            comment_count: self.descendants,
            text: None,
        };

        Ok(match item_type.as_str() {
            "story" => FeedEntry::Story(details),
            "job" => FeedEntry::Job(details),
            "poll" => FeedEntry::Poll {
                details,
                options: Vec::new(),
            },
            _ => FeedEntry::Unsupported {
                id: requested_id,
                item_type,
                title: details.title,
            },
        })
    }
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum StringOrOther {
        String(String),
        Other(serde::de::IgnoredAny),
    }

    Ok(match Option::<StringOrOther>::deserialize(deserializer)? {
        Some(StringOrOther::String(value)) => Some(value),
        Some(StringOrOther::Other(_)) | None => None,
    })
}

fn terminal_safe_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        .collect()
}

fn url_source(value: &str) -> Option<String> {
    Url::parse(value).ok()?.host_str().map(str::to_owned)
}

async fn fetch_json<T>(client: &Client, url: &str) -> Result<T, ContentError>
where
    T: serde::de::DeserializeOwned,
{
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| ContentError::Retryable)?;
    let status = response.status();
    if !status.is_success() {
        return Err(classify_status(status));
    }
    response.json().await.map_err(|_| ContentError::Unavailable)
}

fn classify_status(status: StatusCode) -> ContentError {
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        ContentError::Retryable
    } else {
        ContentError::Unavailable
    }
}

enum CacheEntry<V> {
    Ready(V),
    Loading(Vec<oneshot::Sender<Result<V, ContentError>>>),
}

struct SharedCache<K, V> {
    entries: Mutex<HashMap<K, CacheEntry<V>>>,
}

impl<K, V> Default for SharedCache<K, V> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<K, V> SharedCache<K, V>
where
    K: Clone + Eq + Hash + Send + 'static,
    V: Clone + Send + 'static,
{
    async fn get_or_fetch<F>(self: &Arc<Self>, key: K, fetch: F) -> Result<V, ContentError>
    where
        F: Future<Output = Result<V, ContentError>> + Send + 'static,
    {
        let (sender, receiver) = oneshot::channel();
        let should_fetch = {
            let mut entries = self.entries.lock().await;
            match entries.get_mut(&key) {
                Some(CacheEntry::Ready(value)) => return Ok(value.clone()),
                Some(CacheEntry::Loading(waiters)) => {
                    waiters.push(sender);
                    false
                }
                None => {
                    entries.insert(key.clone(), CacheEntry::Loading(vec![sender]));
                    true
                }
            }
        };

        if should_fetch {
            let cache = Arc::clone(self);
            tokio::spawn(async move {
                let result = fetch.await;
                cache.complete(key, result).await;
            });
        }

        receiver.await.unwrap_or(Err(ContentError::Retryable))
    }

    async fn complete(&self, key: K, result: Result<V, ContentError>) {
        let waiters = {
            let mut entries = self.entries.lock().await;
            let waiters = match entries.remove(&key) {
                Some(CacheEntry::Loading(waiters)) => waiters,
                Some(CacheEntry::Ready(_)) | None => return,
            };
            if let Ok(value) = &result {
                entries.insert(key, CacheEntry::Ready(value.clone()));
            }
            waiters
        };

        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }
}
