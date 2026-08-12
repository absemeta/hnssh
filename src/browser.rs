use crate::content::{ContentError, FeedEntry, FeedEntryId, Link};
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
    Ready(FeedEntry),
    Unavailable,
    Retryable,
}

#[derive(Clone, Debug)]
pub(crate) struct FeedRow {
    pub(crate) rank: usize,
    pub(crate) id: FeedEntryId,
    pub(crate) state: FeedRowState,
}

impl FeedRow {
    pub(crate) fn title(&self) -> &str {
        match &self.state {
            FeedRowState::Loading => "Loading feed entry…",
            FeedRowState::Ready(entry) => entry.title(),
            FeedRowState::Unavailable => "[feed entry unavailable]",
            FeedRowState::Retryable => "[feed entry temporarily unavailable]",
        }
    }

    pub(crate) fn entry(&self) -> Option<&FeedEntry> {
        match &self.state {
            FeedRowState::Ready(entry) => Some(entry),
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
pub(crate) enum CommandOutcome {
    Redraw,
    ReloadTopFeed,
    Unchanged,
    Quit,
}

#[derive(Debug)]
pub(crate) struct Browser {
    feed_status: FeedStatus,
    rows: Vec<FeedRow>,
    views: Vec<ViewState>,
}

impl Browser {
    pub(crate) fn new() -> Self {
        Self {
            feed_status: FeedStatus::Loading,
            rows: Vec::new(),
            views: vec![ViewState::Feed {
                selected: 0,
                offset: 0,
            }],
        }
    }

    pub(crate) fn set_top_feed(&mut self, result: Result<Vec<FeedEntryId>, ContentError>) {
        match result {
            Ok(ids) => {
                self.feed_status = FeedStatus::Ready;
                self.rows = ids
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| FeedRow {
                        rank: index + 1,
                        id,
                        state: FeedRowState::Loading,
                    })
                    .collect();
                self.clamp_feed_selection(1);
            }
            Err(_) => {
                self.feed_status = FeedStatus::Unavailable;
                self.rows.clear();
                self.clamp_feed_selection(1);
            }
        }
    }

    pub(crate) fn set_feed_entry(
        &mut self,
        index: usize,
        id: FeedEntryId,
        result: Result<FeedEntry, ContentError>,
    ) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        if row.id != id {
            return;
        }

        row.state = match result {
            Ok(FeedEntry::Unavailable { id: entry_id }) if entry_id == id => {
                FeedRowState::Unavailable
            }
            Ok(entry) if entry.id() == id => FeedRowState::Ready(entry),
            Err(ContentError::Retryable) => FeedRowState::Retryable,
            Ok(_) | Err(ContentError::Unavailable) => FeedRowState::Unavailable,
        };
    }

    pub(crate) fn feed_status(&self) -> FeedStatus {
        self.feed_status
    }

    pub(crate) fn rows(&self) -> &[FeedRow] {
        &self.rows
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
        if key == Key::Refresh
            && self.view_kind() == ViewKind::Feed
            && self.feed_status == FeedStatus::Unavailable
        {
            self.feed_status = FeedStatus::Loading;
            return CommandOutcome::ReloadTopFeed;
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
            if self.rows.get(selected).and_then(FeedRow::entry).is_some() {
                self.views.push(ViewState::Detail {
                    row: selected,
                    scroll: 0,
                });
                return CommandOutcome::Redraw;
            }
            return CommandOutcome::Unchanged;
        }

        let last = self.rows.len().saturating_sub(1);
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
        let last = self.rows.len().saturating_sub(1);
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

    #[tokio::test]
    async fn loading_resolution_keeps_rank_selection_and_back_stack_state() {
        let fixtures = FixtureContent::at(SystemTime::UNIX_EPOCH);
        let ids = fixtures.top_feed().await.expect("fixture feed");
        let mut browser = Browser::new();
        browser.set_top_feed(Ok(ids.clone()));

        browser.handle_key(Key::Down, 2);
        browser.handle_key(Key::Down, 2);
        assert_eq!(browser.feed_position(), (2, 1));

        for (index, id) in ids.into_iter().enumerate() {
            browser.set_feed_entry(index, id, fixtures.feed_entry(id).await);
        }
        assert_eq!(browser.feed_position(), (2, 1));

        assert_eq!(browser.handle_key(Key::Enter, 2), CommandOutcome::Redraw);
        assert_eq!(browser.view_kind(), ViewKind::Detail);
        assert_eq!(browser.handle_key(Key::Back, 2), CommandOutcome::Redraw);
        assert_eq!(browser.feed_position(), (2, 1));
    }
}
