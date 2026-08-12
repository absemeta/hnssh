use std::time::Duration;

use crate::content::{
    ContentError, ContentFreshness, ContentSnapshot, FeedEntry, FeedEntryId, Link,
};
use crate::input::Key;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FeedStatus {
    Loading,
    Ready,
    Unavailable,
}

#[derive(Clone, Debug)]
pub(crate) enum FeedRowState {
    Loading,
    Ready(Box<ContentSnapshot<FeedEntry>>),
    Unavailable,
    Retryable,
}

#[derive(Clone, Debug)]
pub(crate) struct FeedRow {
    pub(crate) rank: usize,
    pub(crate) id: FeedEntryId,
    pub(crate) state: FeedRowState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FeedEntryLoad {
    pub(crate) index: usize,
    pub(crate) id: FeedEntryId,
}

impl FeedRow {
    pub(crate) fn title(&self) -> &str {
        match &self.state {
            FeedRowState::Loading => "Loading feed entry…",
            FeedRowState::Ready(snapshot) => snapshot.value.title(),
            FeedRowState::Unavailable => "[feed entry unavailable]",
            FeedRowState::Retryable => "[feed entry temporarily unavailable]",
        }
    }

    pub(crate) fn entry(&self) -> Option<&FeedEntry> {
        match &self.state {
            FeedRowState::Ready(snapshot) => Some(&snapshot.value),
            FeedRowState::Loading | FeedRowState::Unavailable | FeedRowState::Retryable => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewKind {
    Feed,
    Detail,
    Links,
    Help,
}

#[derive(Clone, Debug)]
enum ViewState {
    Feed {
        selected: usize,
        offset: usize,
    },
    Detail {
        row: usize,
        scroll: u16,
    },
    Links {
        row: usize,
        selected: usize,
        offset: usize,
    },
    Help {
        scroll: u16,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ContentUpdate {
    Initial,
    Revalidation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommandOutcome {
    Redraw,
    RefreshTopFeed,
    LoadNextPage,
    Unchanged,
    Quit,
}

#[derive(Debug)]
pub(crate) struct Browser {
    feed_status: FeedStatus,
    top_feed_snapshot: Option<ContentSnapshot<()>>,
    refreshing: bool,
    pending_entry_revalidations: usize,
    refresh_failed: bool,
    all_ids: Vec<FeedEntryId>,
    rows: Vec<FeedRow>,
    views: Vec<ViewState>,
}

impl Browser {
    pub(crate) fn new() -> Self {
        Self {
            feed_status: FeedStatus::Loading,
            top_feed_snapshot: None,
            refreshing: false,
            pending_entry_revalidations: 0,
            refresh_failed: false,
            all_ids: Vec::new(),
            rows: Vec::new(),
            views: vec![ViewState::Feed {
                selected: 0,
                offset: 0,
            }],
        }
    }

    pub(crate) fn set_top_feed(
        &mut self,
        result: Result<ContentSnapshot<Vec<FeedEntryId>>, ContentError>,
        page_size: usize,
        update: ContentUpdate,
    ) {
        if update == ContentUpdate::Initial {
            self.refreshing = false;
            self.pending_entry_revalidations = 0;
        }
        match result {
            Ok(snapshot) => {
                let selected_id = self.rows.get(self.feed_position().0).map(|row| row.id);
                let loaded = if update == ContentUpdate::Revalidation {
                    self.rows.len().max(page_size)
                } else {
                    page_size
                };
                let mut previous_rows = std::mem::take(&mut self.rows);
                let top_feed_snapshot = snapshot.metadata_only();
                self.all_ids = snapshot.value;
                self.rows = self
                    .all_ids
                    .iter()
                    .copied()
                    .take(loaded)
                    .enumerate()
                    .map(|(index, id)| {
                        let previous = previous_rows
                            .iter()
                            .position(|row| row.id == id)
                            .map(|position| previous_rows.remove(position));
                        FeedRow {
                            rank: index + 1,
                            id,
                            state: previous
                                .map(|row| row.state)
                                .unwrap_or(FeedRowState::Loading),
                        }
                    })
                    .collect();
                self.feed_status = FeedStatus::Ready;
                self.top_feed_snapshot = Some(top_feed_snapshot);
                self.refresh_failed = false;
                if update == ContentUpdate::Revalidation {
                    self.pending_entry_revalidations = self.rows.len();
                    self.refreshing = self.pending_entry_revalidations > 0;
                }
                if let Some(selected) =
                    selected_id.and_then(|id| self.rows.iter().position(|row| row.id == id))
                {
                    let ViewState::Feed {
                        selected: current, ..
                    } = self
                        .views
                        .first_mut()
                        .expect("browser always has a root view")
                    else {
                        unreachable!("the root view is always the feed")
                    };
                    *current = selected;
                }
                self.clamp_feed_selection(1);
            }
            Err(_) => {
                self.refreshing = false;
                self.pending_entry_revalidations = 0;
                let usable = !self.rows.is_empty()
                    && self
                        .top_feed_snapshot
                        .as_ref()
                        .is_some_and(ContentSnapshot::usable_after_failure);
                if usable {
                    self.feed_status = FeedStatus::Ready;
                    if let Some(snapshot) = &mut self.top_feed_snapshot {
                        snapshot.mark_stale();
                    }
                } else {
                    self.feed_status = FeedStatus::Unavailable;
                    self.top_feed_snapshot = None;
                    self.all_ids.clear();
                    self.rows.clear();
                    self.clamp_feed_selection(1);
                }
                self.refresh_failed = true;
            }
        }
    }

    pub(crate) fn set_feed_entry(
        &mut self,
        index: usize,
        id: FeedEntryId,
        result: Result<ContentSnapshot<FeedEntry>, ContentError>,
        update: ContentUpdate,
    ) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        if row.id != id {
            return;
        }

        let preserve = update == ContentUpdate::Revalidation
            && result.is_err()
            && matches!(
                &row.state,
                FeedRowState::Ready(snapshot) if snapshot.usable_after_failure()
            );
        if preserve {
            let FeedRowState::Ready(snapshot) = &mut row.state else {
                unreachable!("the preserve guard requires a ready row")
            };
            snapshot.mark_stale();
        } else {
            row.state = match result {
                Ok(ContentSnapshot {
                    value: FeedEntry::Unavailable { id: entry_id },
                    ..
                }) if entry_id == id => FeedRowState::Unavailable,
                Ok(snapshot) if snapshot.value.id() == id => {
                    FeedRowState::Ready(Box::new(snapshot))
                }
                Err(ContentError::Retryable) => FeedRowState::Retryable,
                Ok(_) | Err(ContentError::Unavailable) => FeedRowState::Unavailable,
            };
        }

        if update == ContentUpdate::Revalidation {
            self.pending_entry_revalidations = self.pending_entry_revalidations.saturating_sub(1);
            self.refreshing = self.pending_entry_revalidations > 0;
        }
    }

    pub(crate) fn feed_status(&self) -> FeedStatus {
        self.feed_status
    }

    pub(crate) fn top_feed_freshness(&self) -> Option<ContentFreshness> {
        self.top_feed_snapshot
            .as_ref()
            .map(|snapshot| snapshot.freshness)
    }

    pub(crate) fn top_feed_age(&self) -> Duration {
        self.top_feed_snapshot
            .as_ref()
            .map(ContentSnapshot::current_age)
            .unwrap_or(Duration::ZERO)
    }

    pub(crate) fn is_refreshing(&self) -> bool {
        self.refreshing
    }

    pub(crate) fn begin_top_feed_revalidation(&mut self) {
        self.refreshing = true;
        self.pending_entry_revalidations = 0;
        self.refresh_failed = false;
        if self.rows.is_empty() {
            self.feed_status = FeedStatus::Loading;
        }
    }

    pub(crate) fn begin_entry_revalidation(&mut self) {
        self.pending_entry_revalidations = self.pending_entry_revalidations.saturating_add(1);
        self.refreshing = true;
    }

    pub(crate) fn refresh_failed(&self) -> bool {
        self.refresh_failed
    }

    pub(crate) fn rows(&self) -> &[FeedRow] {
        &self.rows
    }

    pub(crate) fn has_more(&self) -> bool {
        self.rows.len() < self.all_ids.len()
    }

    pub(crate) fn remaining_count(&self) -> usize {
        self.all_ids.len().saturating_sub(self.rows.len())
    }

    pub(crate) fn selectable_feed_len(&self) -> usize {
        self.rows.len() + usize::from(self.has_more())
    }

    pub(crate) fn loaded_entries(&self) -> Vec<FeedEntryLoad> {
        self.rows
            .iter()
            .enumerate()
            .map(|(index, row)| FeedEntryLoad { index, id: row.id })
            .collect()
    }

    pub(crate) fn load_next_page(&mut self, page_size: usize) -> Vec<FeedEntryLoad> {
        let start = self.rows.len();
        let end = start.saturating_add(page_size).min(self.all_ids.len());
        self.rows
            .extend(
                self.all_ids[start..end]
                    .iter()
                    .copied()
                    .enumerate()
                    .map(|(offset, id)| FeedRow {
                        rank: start + offset + 1,
                        id,
                        state: FeedRowState::Loading,
                    }),
            );
        (start..end)
            .map(|index| FeedEntryLoad {
                index,
                id: self.all_ids[index],
            })
            .collect()
    }

    pub(crate) fn view_kind(&self) -> ViewKind {
        match self.views.last().expect("browser always has a root view") {
            ViewState::Feed { .. } => ViewKind::Feed,
            ViewState::Detail { .. } => ViewKind::Detail,
            ViewState::Links { .. } => ViewKind::Links,
            ViewState::Help { .. } => ViewKind::Help,
        }
    }

    pub(crate) fn feed_position(&self) -> (usize, usize) {
        match self.views.first().expect("browser always has a root view") {
            ViewState::Feed { selected, offset } => (*selected, *offset),
            _ => unreachable!("the root view is always the feed"),
        }
    }

    pub(crate) fn current_entry(&self) -> Option<&FeedEntry> {
        let row = match self.views.last()? {
            ViewState::Detail { row, .. } | ViewState::Links { row, .. } => *row,
            ViewState::Feed { selected, .. } => *selected,
            ViewState::Help { .. } => return None,
        };
        self.rows.get(row)?.entry()
    }

    pub(crate) fn current_scroll(&self) -> u16 {
        match self.views.last().expect("browser always has a root view") {
            ViewState::Detail { scroll, .. } | ViewState::Help { scroll } => *scroll,
            ViewState::Feed { .. } | ViewState::Links { .. } => 0,
        }
    }

    pub(crate) fn link_position(&self) -> (usize, usize) {
        match self.views.last().expect("browser always has a root view") {
            ViewState::Links {
                selected, offset, ..
            } => (*selected, *offset),
            _ => (0, 0),
        }
    }

    pub(crate) fn current_links(&self) -> &[Link] {
        self.current_entry().map_or(&[], FeedEntry::links)
    }

    pub(crate) fn handle_key(&mut self, key: Key, page_len: usize) -> CommandOutcome {
        if key == Key::Quit {
            return CommandOutcome::Quit;
        }
        if key == Key::Refresh && self.view_kind() == ViewKind::Feed {
            if self.refreshing {
                return CommandOutcome::Unchanged;
            }
            self.begin_top_feed_revalidation();
            return CommandOutcome::RefreshTopFeed;
        }
        if key == Key::Help {
            if self.view_kind() == ViewKind::Help {
                return CommandOutcome::Unchanged;
            }
            self.views.push(ViewState::Help { scroll: 0 });
            return CommandOutcome::Redraw;
        }
        if key == Key::Back {
            if self.views.len() > 1 {
                self.views.pop();
                return CommandOutcome::Redraw;
            }
            return CommandOutcome::Unchanged;
        }

        match self.view_kind() {
            ViewKind::Feed => self.handle_feed_key(key, page_len.max(1)),
            ViewKind::Detail => self.handle_detail_key(key, page_len.max(1)),
            ViewKind::Links => self.handle_links_key(key, page_len.max(1)),
            ViewKind::Help => self.handle_help_key(key, page_len.max(1)),
        }
    }

    fn handle_feed_key(&mut self, key: Key, page_len: usize) -> CommandOutcome {
        if key == Key::Enter {
            let (selected, _) = self.feed_position();
            if selected == self.rows.len() && self.has_more() {
                return CommandOutcome::LoadNextPage;
            }
            if self.rows.get(selected).and_then(FeedRow::entry).is_some() {
                self.views.push(ViewState::Detail {
                    row: selected,
                    scroll: 0,
                });
                return CommandOutcome::Redraw;
            }
            return CommandOutcome::Unchanged;
        }

        let last = self.selectable_feed_len().saturating_sub(1);
        let ViewState::Feed { selected, offset } = self
            .views
            .first_mut()
            .expect("browser always has a root view")
        else {
            unreachable!("the root view is always the feed")
        };
        let previous = (*selected, *offset);
        *selected = match key {
            Key::Up => selected.saturating_sub(1),
            Key::Down => selected.saturating_add(1).min(last),
            Key::PageUp => selected.saturating_sub(page_len),
            Key::PageDown => selected.saturating_add(page_len).min(last),
            Key::First => 0,
            Key::Last => last,
            _ => return CommandOutcome::Unchanged,
        };
        keep_visible(*selected, offset, page_len);

        if previous == (*selected, *offset) {
            CommandOutcome::Unchanged
        } else {
            CommandOutcome::Redraw
        }
    }

    fn handle_detail_key(&mut self, key: Key, page_len: usize) -> CommandOutcome {
        if key == Key::Enter {
            let Some(ViewState::Detail { row, .. }) = self.views.last() else {
                unreachable!()
            };
            let row = *row;
            if self
                .rows
                .get(row)
                .and_then(FeedRow::entry)
                .is_some_and(|entry| !entry.links().is_empty())
            {
                self.views.push(ViewState::Links {
                    row,
                    selected: 0,
                    offset: 0,
                });
                return CommandOutcome::Redraw;
            }
            return CommandOutcome::Unchanged;
        }

        let ViewState::Detail { scroll, .. } = self.views.last_mut().expect("detail exists") else {
            unreachable!()
        };
        update_scroll(scroll, key, page_len, 64)
    }

    fn handle_links_key(&mut self, key: Key, page_len: usize) -> CommandOutcome {
        let link_count = self.current_links().len();
        let last = link_count.saturating_sub(1);
        let ViewState::Links {
            selected, offset, ..
        } = self.views.last_mut().expect("links exists")
        else {
            unreachable!()
        };
        let previous = (*selected, *offset);
        *selected = match key {
            Key::Up => selected.saturating_sub(1),
            Key::Down => selected.saturating_add(1).min(last),
            Key::PageUp => selected.saturating_sub(page_len),
            Key::PageDown => selected.saturating_add(page_len).min(last),
            Key::First => 0,
            Key::Last => last,
            _ => return CommandOutcome::Unchanged,
        };
        keep_visible(*selected, offset, page_len);

        if previous == (*selected, *offset) {
            CommandOutcome::Unchanged
        } else {
            CommandOutcome::Redraw
        }
    }

    fn handle_help_key(&mut self, key: Key, page_len: usize) -> CommandOutcome {
        let ViewState::Help { scroll } = self.views.last_mut().expect("help exists") else {
            unreachable!()
        };
        update_scroll(scroll, key, page_len, 8)
    }

    fn clamp_feed_selection(&mut self, page_len: usize) {
        let last = self.selectable_feed_len().saturating_sub(1);
        let ViewState::Feed { selected, offset } = self
            .views
            .first_mut()
            .expect("browser always has a root view")
        else {
            unreachable!()
        };
        *selected = (*selected).min(last);
        keep_visible(*selected, offset, page_len);
    }
}

fn keep_visible(selected: usize, offset: &mut usize, page_len: usize) {
    if selected < *offset {
        *offset = selected;
    } else if selected >= offset.saturating_add(page_len) {
        *offset = selected + 1 - page_len;
    }
}

fn update_scroll(scroll: &mut u16, key: Key, page_len: usize, maximum: u16) -> CommandOutcome {
    let previous = *scroll;
    let page_len = u16::try_from(page_len).unwrap_or(u16::MAX);
    *scroll = match key {
        Key::Up => scroll.saturating_sub(1),
        Key::Down => scroll.saturating_add(1).min(maximum),
        Key::PageUp => scroll.saturating_sub(page_len),
        Key::PageDown => scroll.saturating_add(page_len).min(maximum),
        Key::First => 0,
        Key::Last => maximum,
        _ => return CommandOutcome::Unchanged,
    };

    if previous == *scroll {
        CommandOutcome::Unchanged
    } else {
        CommandOutcome::Redraw
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use crate::content::{ContentSource, FixtureContent};

    use super::*;

    #[test]
    fn expired_session_copy_is_removed_after_refresh_failure() {
        let mut browser = Browser::new();
        browser.set_top_feed(
            Ok(ContentSnapshot::cached(
                vec![101],
                ContentFreshness::Fresh,
                Duration::from_millis(20),
                Duration::from_millis(10),
            )),
            30,
            ContentUpdate::Initial,
        );

        browser.set_top_feed(
            Err(ContentError::Retryable),
            30,
            ContentUpdate::Revalidation,
        );

        assert_eq!(browser.feed_status(), FeedStatus::Unavailable);
        assert!(browser.rows().is_empty());
    }

    #[tokio::test]
    async fn loading_resolution_keeps_rank_selection_and_back_stack_state() {
        let fixtures = FixtureContent::at(SystemTime::UNIX_EPOCH);
        let ids = fixtures.top_feed().await.expect("fixture feed");
        let mut browser = Browser::new();
        browser.set_top_feed(Ok(ids.clone()), 30, ContentUpdate::Initial);

        browser.handle_key(Key::Down, 2);
        browser.handle_key(Key::Down, 2);
        assert_eq!(browser.feed_position(), (2, 1));

        for (index, id) in ids.value.into_iter().enumerate() {
            browser.set_feed_entry(
                index,
                id,
                fixtures.feed_entry(id).await,
                ContentUpdate::Initial,
            );
        }
        assert_eq!(browser.feed_position(), (2, 1));

        assert_eq!(browser.handle_key(Key::Enter, 2), CommandOutcome::Redraw);
        assert_eq!(browser.view_kind(), ViewKind::Detail);
        assert_eq!(browser.handle_key(Key::Back, 2), CommandOutcome::Redraw);
        assert_eq!(browser.feed_position(), (2, 1));
    }
}
