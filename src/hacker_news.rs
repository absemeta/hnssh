use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::{Client, Response, StatusCode, header};
use serde::Deserialize;
use tokio::sync::{Mutex, Semaphore, oneshot};
use tokio::time::{Instant, sleep};

use crate::content::{
    ContentError, ContentFreshness, ContentFuture, ContentSnapshot, ContentSource, EntryDetails,
    FeedEntry, FeedEntryId, Link, PollOption, PollOptionStatus, RichText,
};
use crate::content_safety::{normalize_html, normalize_inline_text, validated_url};

const PRODUCTION_ENDPOINT: &str = "https://hacker-news.firebaseio.com/v0";

/// Resource, retry, and cache bounds for the Hacker News adapter.
#[derive(Clone, Debug)]
pub struct HackerNewsOptions {
    pub request_timeout: Duration,
    pub retry_base_delay: Duration,
    pub maximum_retry_delay: Duration,
    pub top_feed_fresh_for: Duration,
    pub top_feed_stale_for: Duration,
    pub item_fresh_for: Duration,
    pub comment_fresh_for: Duration,
    pub poll_option_fresh_for: Duration,
    pub item_stale_for: Duration,
    pub revalidation_interval: Duration,
    pub maximum_cache_entries: usize,
    pub maximum_cache_weight: usize,
    pub upstream_concurrency: usize,
}

impl Default for HackerNewsOptions {
    fn default() -> Self {
        Self {
            request_timeout: Duration::from_secs(5),
            retry_base_delay: Duration::from_millis(100),
            maximum_retry_delay: Duration::from_secs(2),
            top_feed_fresh_for: Duration::from_secs(60),
            top_feed_stale_for: Duration::from_secs(15 * 60),
            item_fresh_for: Duration::from_secs(2 * 60),
            comment_fresh_for: Duration::from_secs(5 * 60),
            poll_option_fresh_for: Duration::from_secs(5 * 60),
            item_stale_for: Duration::from_secs(30 * 60),
            revalidation_interval: Duration::from_secs(10),
            maximum_cache_entries: 20_000,
            maximum_cache_weight: 64 * 1024 * 1024,
            upstream_concurrency: 16,
        }
    }
}

/// Official Hacker News v0 content backed by one bounded process-wide cache.
///
/// Production construction fixes the endpoint and resource bounds. Tests may
/// use [`HackerNewsContent::for_endpoint_with_options`] against a controllable
/// local HTTP service with shorter timing windows.
#[derive(Clone)]
pub struct HackerNewsContent {
    client: Client,
    endpoint: String,
    options: Arc<HackerNewsOptions>,
    upstream_limit: Arc<Semaphore>,
    cache: Arc<SharedCache<ResourceKey, ResourceValue>>,
}

impl HackerNewsContent {
    pub fn production() -> Self {
        Self::for_endpoint(PRODUCTION_ENDPOINT)
    }

    pub fn for_endpoint(endpoint: impl Into<String>) -> Self {
        Self::for_endpoint_with_options(endpoint, HackerNewsOptions::default())
    }

    pub fn for_endpoint_with_options(
        endpoint: impl Into<String>,
        mut options: HackerNewsOptions,
    ) -> Self {
        options.maximum_cache_entries = options.maximum_cache_entries.max(1);
        options.maximum_cache_weight = options.maximum_cache_weight.max(1);
        options.upstream_concurrency = options.upstream_concurrency.max(1);
        let client = Client::builder()
            .timeout(options.request_timeout)
            .build()
            .expect("static Hacker News HTTP client configuration is valid");
        let cache = Arc::new(SharedCache::new(
            options.maximum_cache_entries,
            options.maximum_cache_weight,
            options.revalidation_interval,
        ));
        let upstream_limit = Arc::new(Semaphore::new(options.upstream_concurrency));

        Self {
            client,
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            options: Arc::new(options),
            upstream_limit,
            cache,
        }
    }

    fn top_feed_request(
        &self,
        request: CacheRequest,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>> {
        Box::pin(async move {
            let client = self.client.clone();
            let limit = Arc::clone(&self.upstream_limit);
            let options = Arc::clone(&self.options);
            let url = format!("{}/topstories.json", self.endpoint);
            let policy = CachePolicy {
                fresh_for: self.options.top_feed_fresh_for,
                stale_for: self.options.top_feed_stale_for,
            };
            let snapshot = self
                .cache
                .read(ResourceKey::TopFeed, policy, request, async move {
                    fetch_json(&client, &limit, &url, &options)
                        .await
                        .map(ResourceValue::TopFeed)
                })
                .await?;
            snapshot.try_map(|resource| match resource {
                ResourceValue::TopFeed(ids) => Ok(ids),
                ResourceValue::Item { .. } => Err(ContentError::Unavailable),
            })
        })
    }

    async fn item_request(
        &self,
        id: FeedEntryId,
        request: CacheRequest,
    ) -> Result<ContentSnapshot<ResourceValue>, ContentError> {
        let client = self.client.clone();
        let limit = Arc::clone(&self.upstream_limit);
        let options = Arc::clone(&self.options);
        let url = format!("{}/item/{id}.json", self.endpoint);
        let policy = CachePolicy {
            fresh_for: self.options.item_fresh_for,
            stale_for: self.options.item_stale_for,
        };
        self.cache
            .read(ResourceKey::Item(id), policy, request, async move {
                let item: Option<UpstreamItem> =
                    fetch_json(&client, &limit, &url, &options).await?;
                let fresh_for = item.as_ref().map_or(options.item_fresh_for, |item| {
                    item.cache_fresh_for(&options)
                });
                let item = item.map_or_else(
                    || Ok(CachedItem::unavailable(id)),
                    |item| item.into_cached_item(id),
                )?;
                Ok(ResourceValue::Item {
                    item: Box::new(item),
                    fresh_for,
                })
            })
            .await
    }

    async fn poll_option_request(&self, id: FeedEntryId, request: CacheRequest) -> PollOption {
        match self.item_request(id, request).await {
            Ok(snapshot) => {
                let freshness = snapshot.freshness;
                let age = snapshot.current_age();
                let option = match snapshot.value {
                    ResourceValue::Item { item, .. } => item.poll_option,
                    ResourceValue::TopFeed(_) => None,
                };
                match option {
                    Some(mut option) => {
                        if freshness.is_stale() {
                            option.status = PollOptionStatus::Stale { age };
                        }
                        option
                    }
                    None => PollOption::placeholder(PollOptionStatus::Unavailable),
                }
            }
            Err(ContentError::Retryable) => PollOption::placeholder(PollOptionStatus::Retryable),
            Err(ContentError::Unavailable) => {
                PollOption::placeholder(PollOptionStatus::Unavailable)
            }
        }
    }

    fn feed_entry_request(
        &self,
        id: FeedEntryId,
        request: CacheRequest,
    ) -> ContentFuture<'_, Result<ContentSnapshot<FeedEntry>, ContentError>> {
        Box::pin(async move {
            let snapshot = self.item_request(id, request).await?;
            let mut poll_option_ids = Vec::new();
            let mut entry = snapshot.try_map(|resource| match resource {
                ResourceValue::Item { item, .. } => {
                    poll_option_ids = item.poll_option_ids;
                    Ok(item.entry)
                }
                ResourceValue::TopFeed(_) => Err(ContentError::Unavailable),
            })?;

            if let FeedEntry::Poll { options, .. } = &mut entry.value {
                for option_id in poll_option_ids {
                    options.push(self.poll_option_request(option_id, request).await);
                }
            }

            Ok(entry)
        })
    }
}

impl ContentSource for HackerNewsContent {
    fn top_feed(
        &self,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>> {
        self.top_feed_request(CacheRequest::Read)
    }

    fn refresh_top_feed(
        &self,
    ) -> ContentFuture<'_, Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>> {
        self.top_feed_request(CacheRequest::Revalidate)
    }

    fn feed_entry(
        &self,
        id: FeedEntryId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<FeedEntry>, ContentError>> {
        self.feed_entry_request(id, CacheRequest::Read)
    }

    fn refresh_feed_entry(
        &self,
        id: FeedEntryId,
    ) -> ContentFuture<'_, Result<ContentSnapshot<FeedEntry>, ContentError>> {
        self.feed_entry_request(id, CacheRequest::Revalidate)
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
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    text: Option<String>,
    #[serde(default, deserialize_with = "deserialize_optional_ids")]
    parts: Option<Vec<FeedEntryId>>,
    descendants: Option<u32>,
}

#[derive(Clone, Debug)]
struct CachedItem {
    entry: FeedEntry,
    poll_option: Option<PollOption>,
    poll_option_ids: Vec<FeedEntryId>,
}

impl CachedItem {
    fn unavailable(id: FeedEntryId) -> Self {
        Self {
            entry: FeedEntry::Unavailable { id },
            poll_option: None,
            poll_option_ids: Vec::new(),
        }
    }
}

impl UpstreamItem {
    fn cache_fresh_for(&self, options: &HackerNewsOptions) -> Duration {
        match self.item_type.as_deref() {
            Some("comment") => options.comment_fresh_for,
            Some("pollopt") => options.poll_option_fresh_for,
            _ => options.item_fresh_for,
        }
    }

    fn into_cached_item(self, requested_id: FeedEntryId) -> Result<CachedItem, ContentError> {
        if self.id.is_some_and(|id| id != requested_id) {
            return Err(ContentError::Unavailable);
        }
        if self.dead == Some(true) || self.deleted == Some(true) {
            return Ok(CachedItem::unavailable(requested_id));
        }

        let Some(item_type) = self.item_type else {
            return Ok(CachedItem::unavailable(requested_id));
        };
        let title = self.title.as_deref().and_then(normalize_inline_text);
        let author = self.by.as_deref().and_then(normalize_inline_text);
        let normalized_text = self.text.as_deref().map(normalize_html);
        let text = normalized_text
            .as_ref()
            .map(|content| content.text.clone())
            .filter(|text| !text.lines.is_empty());
        let valid_url = self.url.as_deref().and_then(validated_url);
        let source = valid_url.as_ref().and_then(|url| url.source.clone());
        let mut links = Vec::new();
        if let Some(url) = &valid_url {
            links.push(Link::new(
                title.clone().unwrap_or_else(|| url.destination.clone()),
                url.destination.clone(),
            ));
        }
        let mut omitted_link_count = usize::from(self.url.is_some() && valid_url.is_none());
        if let Some(content) = normalized_text {
            omitted_link_count = omitted_link_count.saturating_add(content.omitted_link_count);
            links.extend(content.links);
        }
        let mut details = EntryDetails {
            id: requested_id,
            source,
            links,
            title: title.clone(),
            score: self.score,
            author,
            published_at: self.time.and_then(upstream_timestamp),
            comment_count: self.descendants,
            text,
            omitted_link_count,
        };
        if item_type == "job" {
            details.score = None;
            details.comment_count = None;
        }
        let poll_option = (item_type == "pollopt").then(|| {
            self.text
                .as_deref()
                .and_then(normalize_inline_text)
                .map(|text| PollOption {
                    text,
                    score: self.score,
                    status: PollOptionStatus::Available,
                })
        });
        let poll_option_ids = if item_type == "poll" {
            self.parts.unwrap_or_default()
        } else {
            Vec::new()
        };

        let entry = match item_type.as_str() {
            "story" => FeedEntry::Story(details),
            "job" => FeedEntry::Job(details),
            "poll" => FeedEntry::Poll {
                details,
                options: Vec::new(),
            },
            _ => FeedEntry::Unsupported {
                id: requested_id,
                item_type,
                title,
            },
        };
        Ok(CachedItem {
            entry,
            poll_option: poll_option.flatten(),
            poll_option_ids,
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

fn deserialize_optional_ids<'de, D>(deserializer: D) -> Result<Option<Vec<FeedEntryId>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum IdsOrOther {
        Ids(Vec<FeedEntryId>),
        Other(serde::de::IgnoredAny),
    }

    Ok(match Option::<IdsOrOther>::deserialize(deserializer)? {
        Some(IdsOrOther::Ids(ids)) => Some(ids),
        Some(IdsOrOther::Other(_)) | None => None,
    })
}

fn upstream_timestamp(seconds: u64) -> Option<SystemTime> {
    const LAST_SUPPORTED_SECOND: u64 = 253_402_300_799;
    (seconds <= LAST_SUPPORTED_SECOND)
        .then(|| SystemTime::UNIX_EPOCH.checked_add(Duration::from_secs(seconds)))
        .flatten()
}

async fn fetch_json<T>(
    client: &Client,
    limit: &Semaphore,
    url: &str,
    options: &HackerNewsOptions,
) -> Result<T, ContentError>
where
    T: serde::de::DeserializeOwned,
{
    for attempt in 0..=1 {
        let permit = limit.acquire().await.map_err(|_| ContentError::Retryable)?;
        let response = client.get(url).send().await;
        match response {
            Ok(response) if response.status().is_success() => {
                let result = response.json().await.map_err(|_| ContentError::Unavailable);
                drop(permit);
                return result;
            }
            Ok(response) if retryable_status(response.status()) && attempt == 0 => {
                let delay = retry_delay(&response, options);
                drop(permit);
                sleep(delay).await;
            }
            Ok(response) => {
                let error = if retryable_status(response.status()) {
                    ContentError::Retryable
                } else {
                    ContentError::Unavailable
                };
                drop(permit);
                return Err(error);
            }
            Err(_) if attempt == 0 => {
                drop(permit);
                sleep(jittered_delay(
                    options.retry_base_delay,
                    options.maximum_retry_delay,
                ))
                .await;
            }
            Err(_) => {
                drop(permit);
                return Err(ContentError::Retryable);
            }
        }
    }
    unreachable!("the retry loop always returns")
}

fn retryable_status(status: StatusCode) -> bool {
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn retry_delay(response: &Response, options: &HackerNewsOptions) -> Duration {
    let requested = response
        .headers()
        .get(header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after)
        .unwrap_or(options.retry_base_delay)
        .min(options.maximum_retry_delay);
    jittered_delay(requested, options.maximum_retry_delay)
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let requested_at = httpdate::parse_http_date(value).ok()?;
    Some(
        requested_at
            .duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn jittered_delay(base: Duration, maximum: Duration) -> Duration {
    let jitter_bound = base / 4;
    if jitter_bound.is_zero() {
        return base.min(maximum);
    }
    let nanos = u64::from(
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .subsec_nanos(),
    );
    let bound_nanos = jitter_bound.as_nanos().min(u128::from(u64::MAX)) as u64;
    let jitter = Duration::from_nanos(nanos % bound_nanos.max(1));
    base.saturating_add(jitter).min(maximum)
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum ResourceKey {
    TopFeed,
    Item(FeedEntryId),
}

#[derive(Clone, Debug)]
enum ResourceValue {
    TopFeed(Vec<FeedEntryId>),
    Item {
        item: Box<CachedItem>,
        fresh_for: Duration,
    },
}

impl ResourceValue {
    fn estimated_weight(&self) -> usize {
        match self {
            Self::TopFeed(ids) => 64usize.saturating_add(ids.len().saturating_mul(8)),
            Self::Item { item, .. } => cached_item_weight(item),
        }
    }
}

fn cached_item_weight(item: &CachedItem) -> usize {
    let mut weight =
        feed_entry_weight(&item.entry).saturating_add(item.poll_option_ids.len().saturating_mul(8));
    if let Some(option) = &item.poll_option {
        weight = weight.saturating_add(option.text.len()).saturating_add(16);
    }
    weight
}

fn feed_entry_weight(entry: &FeedEntry) -> usize {
    let mut weight = 128usize;
    if let Some(details) = entry.details() {
        for value in [
            details.title.as_deref(),
            details.source.as_deref(),
            details.author.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            weight = weight.saturating_add(value.len());
        }
        if let Some(text) = &details.text {
            weight = weight.saturating_add(rich_text_weight(text));
        }
        for link in &details.links {
            weight = weight
                .saturating_add(link.label.len())
                .saturating_add(link.destination.len());
        }
    }
    match entry {
        FeedEntry::Poll { options, .. } => {
            for option in options {
                weight = weight.saturating_add(option.text.len()).saturating_add(16);
            }
        }
        FeedEntry::Unsupported {
            item_type, title, ..
        } => {
            weight = weight.saturating_add(item_type.len());
            if let Some(title) = title {
                weight = weight.saturating_add(title.len());
            }
        }
        FeedEntry::Story(_) | FeedEntry::Job(_) | FeedEntry::Unavailable { .. } => {}
    }
    weight
}

fn rich_text_weight(text: &RichText) -> usize {
    text.lines.iter().fold(0usize, |weight, line| {
        line.spans.iter().fold(weight, |weight, span| {
            weight.saturating_add(span.text.len()).saturating_add(8)
        })
    })
}

#[derive(Clone, Copy)]
struct CachePolicy {
    fresh_for: Duration,
    stale_for: Duration,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CacheRequest {
    Read,
    Revalidate,
}

struct ReadyEntry<V> {
    value: V,
    fetched_at: Instant,
    fresh_for: Duration,
    last_access: u64,
    weight: usize,
    last_revalidation: Option<Instant>,
    known_stale: bool,
}

struct LoadingEntry<V> {
    stale: Option<ReadyEntry<V>>,
    waiters: Vec<oneshot::Sender<Result<ContentSnapshot<V>, ContentError>>>,
    revalidation_started: Option<Instant>,
}

struct FailedEntry {
    error: ContentError,
    last_revalidation: Instant,
    last_access: u64,
}

enum CacheEntry<V> {
    Ready(ReadyEntry<V>),
    Loading(LoadingEntry<V>),
    Failed(FailedEntry),
}

struct CacheState<K, V> {
    entries: HashMap<K, CacheEntry<V>>,
    total_weight: usize,
    access_clock: u64,
}

impl<K, V> Default for CacheState<K, V> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            total_weight: 0,
            access_clock: 0,
        }
    }
}

impl<K, V> CacheState<K, V> {
    fn next_access(&mut self) -> u64 {
        self.access_clock = self.access_clock.wrapping_add(1);
        self.access_clock
    }
}

struct SharedCache<K, V> {
    state: Mutex<CacheState<K, V>>,
    maximum_entries: usize,
    maximum_weight: usize,
    revalidation_interval: Duration,
}

impl<K, V> SharedCache<K, V>
where
    K: Clone + Eq + Hash + Send + 'static,
    V: Clone + Send + 'static,
{
    fn new(maximum_entries: usize, maximum_weight: usize, revalidation_interval: Duration) -> Self {
        Self {
            state: Mutex::new(CacheState::default()),
            maximum_entries,
            maximum_weight,
            revalidation_interval,
        }
    }

    async fn read<F>(
        self: &Arc<Self>,
        key: K,
        policy: CachePolicy,
        request: CacheRequest,
        fetch: F,
    ) -> Result<ContentSnapshot<V>, ContentError>
    where
        F: Future<Output = Result<V, ContentError>> + Send + 'static,
        V: CacheWeight,
    {
        let now = Instant::now();
        let (sender, receiver) = oneshot::channel();
        let mut sender = Some(sender);
        let mut immediate = None;
        let mut should_fetch = false;

        {
            let mut state = self.state.lock().await;
            match state.entries.remove(&key) {
                None => {
                    state.entries.insert(
                        key.clone(),
                        CacheEntry::Loading(LoadingEntry {
                            stale: None,
                            waiters: vec![sender.take().expect("new request waiter")],
                            revalidation_started: (request == CacheRequest::Revalidate)
                                .then_some(now),
                        }),
                    );
                    should_fetch = true;
                }
                Some(CacheEntry::Ready(mut ready)) => {
                    let age = now.saturating_duration_since(ready.fetched_at);
                    let usable = age <= policy.stale_for;
                    let needs_revalidation = ready.known_stale || age > ready.fresh_for;
                    let throttled = ready.last_revalidation.is_some_and(|last| {
                        now.saturating_duration_since(last) < self.revalidation_interval
                    });

                    if (request == CacheRequest::Read && !needs_revalidation)
                        || (usable && throttled)
                    {
                        ready.last_access = state.next_access();
                        immediate = Some(Ok(snapshot(&ready, policy, now)));
                        state.entries.insert(key.clone(), CacheEntry::Ready(ready));
                    } else if request == CacheRequest::Read && usable {
                        ready.last_access = state.next_access();
                        immediate = Some(Ok(ContentSnapshot::cached(
                            ready.value.clone(),
                            ContentFreshness::Stale { age },
                            age,
                            policy.stale_for,
                        )));
                        state.entries.insert(key.clone(), CacheEntry::Ready(ready));
                    } else if request == CacheRequest::Revalidate && usable {
                        state.entries.insert(
                            key.clone(),
                            CacheEntry::Loading(LoadingEntry {
                                stale: Some(ready),
                                waiters: vec![sender.take().expect("revalidation waiter")],
                                revalidation_started: Some(now),
                            }),
                        );
                        should_fetch = true;
                    } else if throttled {
                        ready.last_access = state.next_access();
                        state.entries.insert(key.clone(), CacheEntry::Ready(ready));
                        immediate = Some(Err(ContentError::Retryable));
                    } else {
                        state.total_weight = state.total_weight.saturating_sub(ready.weight);
                        state.entries.insert(
                            key.clone(),
                            CacheEntry::Loading(LoadingEntry {
                                stale: None,
                                waiters: vec![sender.take().expect("expired request waiter")],
                                revalidation_started: Some(now),
                            }),
                        );
                        should_fetch = true;
                    }
                }
                Some(CacheEntry::Loading(mut loading)) => {
                    let stale = loading.stale.as_ref().and_then(|ready| {
                        let age = now.saturating_duration_since(ready.fetched_at);
                        (age <= policy.stale_for).then(|| {
                            ContentSnapshot::cached(
                                ready.value.clone(),
                                ContentFreshness::Stale { age },
                                age,
                                policy.stale_for,
                            )
                        })
                    });
                    if request == CacheRequest::Read {
                        if let Some(stale) = stale {
                            immediate = Some(Ok(stale));
                        } else {
                            loading
                                .waiters
                                .push(sender.take().expect("loading request waiter"));
                        }
                    } else {
                        loading
                            .waiters
                            .push(sender.take().expect("loading revalidation waiter"));
                    }
                    state
                        .entries
                        .insert(key.clone(), CacheEntry::Loading(loading));
                }
                Some(CacheEntry::Failed(mut failed)) => {
                    if now.saturating_duration_since(failed.last_revalidation)
                        < self.revalidation_interval
                    {
                        failed.last_access = state.next_access();
                        let error = failed.error;
                        state
                            .entries
                            .insert(key.clone(), CacheEntry::Failed(failed));
                        immediate = Some(Err(error));
                    } else {
                        state.entries.insert(
                            key.clone(),
                            CacheEntry::Loading(LoadingEntry {
                                stale: None,
                                waiters: vec![sender.take().expect("failed resource waiter")],
                                revalidation_started: Some(now),
                            }),
                        );
                        should_fetch = true;
                    }
                }
            }
        }

        if should_fetch {
            let cache = Arc::clone(self);
            tokio::spawn(async move {
                let result = fetch.await;
                cache.complete(key, policy, result).await;
            });
        }

        if let Some(result) = immediate {
            return result;
        }
        receiver.await.unwrap_or(Err(ContentError::Retryable))
    }

    async fn complete(&self, key: K, policy: CachePolicy, result: Result<V, ContentError>)
    where
        V: CacheWeight,
    {
        let now = Instant::now();
        let (waiters, waiter_result) = {
            let mut state = self.state.lock().await;
            let Some(CacheEntry::Loading(loading)) = state.entries.remove(&key) else {
                return;
            };
            let waiters = loading.waiters;

            match result {
                Ok(value) => {
                    if let Some(stale) = loading.stale {
                        state.total_weight = state.total_weight.saturating_sub(stale.weight);
                    }
                    let weight = value.cache_weight();
                    state.total_weight = state.total_weight.saturating_add(weight);
                    let ready = ReadyEntry {
                        value: value.clone(),
                        fetched_at: now,
                        fresh_for: value.cache_fresh_for(policy.fresh_for),
                        last_access: state.next_access(),
                        weight,
                        last_revalidation: loading.revalidation_started,
                        known_stale: false,
                    };
                    state.entries.insert(key, CacheEntry::Ready(ready));
                    self.enforce_bounds(&mut state);
                    (
                        waiters,
                        Ok(ContentSnapshot::cached(
                            value,
                            ContentFreshness::Fresh,
                            Duration::ZERO,
                            policy.stale_for,
                        )),
                    )
                }
                Err(error) => {
                    if let Some(mut stale) = loading.stale {
                        let age = now.saturating_duration_since(stale.fetched_at);
                        if age <= policy.stale_for {
                            stale.known_stale = true;
                            stale.last_revalidation = loading.revalidation_started;
                            stale.last_access = state.next_access();
                            let value = stale.value.clone();
                            state.entries.insert(key, CacheEntry::Ready(stale));
                            self.enforce_bounds(&mut state);
                            (
                                waiters,
                                Ok(ContentSnapshot::cached(
                                    value,
                                    ContentFreshness::Stale { age },
                                    age,
                                    policy.stale_for,
                                )),
                            )
                        } else {
                            state.total_weight = state.total_weight.saturating_sub(stale.weight);
                            if let Some(last_revalidation) = loading.revalidation_started {
                                self.record_failure(&mut state, key, error, last_revalidation);
                            }
                            (waiters, Err(error))
                        }
                    } else {
                        if let Some(last_revalidation) = loading.revalidation_started {
                            self.record_failure(&mut state, key, error, last_revalidation);
                        }
                        (waiters, Err(error))
                    }
                }
            }
        };

        for waiter in waiters {
            let _ = waiter.send(waiter_result.clone());
        }
    }

    fn record_failure(
        &self,
        state: &mut CacheState<K, V>,
        key: K,
        error: ContentError,
        last_revalidation: Instant,
    ) {
        let failed = FailedEntry {
            error,
            last_revalidation,
            last_access: state.next_access(),
        };
        state.entries.insert(key, CacheEntry::Failed(failed));
        self.enforce_bounds(state);
    }

    fn enforce_bounds(&self, state: &mut CacheState<K, V>) {
        loop {
            let inactive_count = state
                .entries
                .values()
                .filter(|entry| !matches!(entry, CacheEntry::Loading(_)))
                .count();
            if inactive_count <= self.maximum_entries && state.total_weight <= self.maximum_weight {
                break;
            }
            let oldest = state
                .entries
                .iter()
                .filter_map(|(key, entry)| match entry {
                    CacheEntry::Ready(ready) => Some((key.clone(), ready.last_access)),
                    CacheEntry::Failed(failed) => Some((key.clone(), failed.last_access)),
                    CacheEntry::Loading(_) => None,
                })
                .min_by_key(|(_, access)| *access)
                .map(|(key, _)| key);
            let Some(oldest) = oldest else {
                break;
            };
            if let Some(CacheEntry::Ready(ready)) = state.entries.remove(&oldest) {
                state.total_weight = state.total_weight.saturating_sub(ready.weight);
            }
        }
    }
}

fn snapshot<V: Clone>(
    ready: &ReadyEntry<V>,
    policy: CachePolicy,
    now: Instant,
) -> ContentSnapshot<V> {
    let age = now.saturating_duration_since(ready.fetched_at);
    let freshness = if ready.known_stale || age > ready.fresh_for {
        ContentFreshness::Stale { age }
    } else {
        ContentFreshness::Fresh
    };
    ContentSnapshot::cached(ready.value.clone(), freshness, age, policy.stale_for)
}

trait CacheWeight {
    fn cache_weight(&self) -> usize;

    fn cache_fresh_for(&self, default: Duration) -> Duration {
        default
    }
}

impl CacheWeight for ResourceValue {
    fn cache_weight(&self) -> usize {
        self.estimated_weight()
    }

    fn cache_fresh_for(&self, default: Duration) -> Duration {
        match self {
            Self::TopFeed(_) => default,
            Self::Item { fresh_for, .. } => *fresh_for,
        }
    }
}
