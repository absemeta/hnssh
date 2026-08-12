use std::collections::HashSet;

use crate::content::{Comment, CommentId, ContentError, ContentSnapshot, FeedEntryId};
use crate::input::Key;

pub(crate) const COMMENT_BATCH_SIZE: usize = 20;
pub(crate) const MAX_THREAD_NODES: usize = 1_000;
pub(crate) const MAX_THREAD_DEPTH: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommentUpdate {
    Initial,
    Revalidation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CommentLoad {
    pub(crate) generation: u64,
    pub(crate) node: usize,
    pub(crate) id: CommentId,
    pub(crate) update: CommentUpdate,
}

#[derive(Clone, Debug)]
pub(crate) enum ThreadNodeState {
    Loading,
    Ready(Box<ContentSnapshot<Comment>>),
    Unavailable,
    Retryable,
}

#[derive(Clone, Debug)]
pub(crate) struct ThreadNode {
    id: CommentId,
    parent: Option<usize>,
    depth: usize,
    state: ThreadNodeState,
    expanded: bool,
    children: Option<SiblingGroup>,
}

impl ThreadNode {
    pub(crate) fn id(&self) -> CommentId {
        self.id
    }

    pub(crate) fn state(&self) -> &ThreadNodeState {
        &self.state
    }

    pub(crate) fn expanded(&self) -> bool {
        self.expanded
    }

    pub(crate) fn has_children(&self) -> bool {
        match &self.children {
            Some(group) => !group.ids.is_empty(),
            None => match &self.state {
                ThreadNodeState::Ready(snapshot) => !snapshot.value.child_ids().is_empty(),
                ThreadNodeState::Loading
                | ThreadNodeState::Unavailable
                | ThreadNodeState::Retryable => false,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GroupOwner {
    Root,
    Node(usize),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ThreadMarker {
    Cycle(CommentId),
    Duplicate(CommentId),
    DepthLimit,
    NodeLimit,
}

impl ThreadMarker {
    fn label(&self) -> String {
        match self {
            Self::Cycle(id) => format!("[cycle to comment {id} omitted]"),
            Self::Duplicate(id) => format!("[duplicate comment {id} omitted]"),
            Self::DepthLimit => format!("[thread depth limit of {MAX_THREAD_DEPTH} reached]"),
            Self::NodeLimit => format!("[thread node limit of {MAX_THREAD_NODES} reached]"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ThreadEdge {
    Node(usize),
    Marker(ThreadMarker),
}

#[derive(Clone, Debug)]
struct SiblingGroup {
    ids: Vec<CommentId>,
    edges: Vec<ThreadEdge>,
    next: usize,
}

impl SiblingGroup {
    fn new(ids: Vec<CommentId>) -> Self {
        Self {
            ids,
            edges: Vec::new(),
            next: 0,
        }
    }

    fn remaining(&self) -> usize {
        self.ids.len().saturating_sub(self.next)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ThreadRow {
    Comment {
        node: usize,
        depth: usize,
    },
    LoadMore {
        owner: GroupOwner,
        depth: usize,
        remaining: usize,
    },
    Marker {
        depth: usize,
        label: String,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct CommentThread {
    entry_id: FeedEntryId,
    generation: u64,
    roots: SiblingGroup,
    nodes: Vec<ThreadNode>,
    seen: HashSet<CommentId>,
    selected: usize,
    offset: usize,
}

impl CommentThread {
    pub(crate) fn open(
        entry_id: FeedEntryId,
        root_ids: Vec<CommentId>,
        generation: u64,
    ) -> (Self, Vec<CommentLoad>) {
        let mut thread = Self {
            entry_id,
            generation,
            roots: SiblingGroup::new(root_ids),
            nodes: Vec::new(),
            seen: HashSet::new(),
            selected: 0,
            offset: 0,
        };
        let loads = thread.load_group(GroupOwner::Root);
        (thread, loads)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn position(&self) -> (usize, usize) {
        (self.selected, self.offset)
    }

    pub(crate) fn node(&self, index: usize) -> Option<&ThreadNode> {
        self.nodes.get(index)
    }

    pub(crate) fn rows(&self) -> Vec<ThreadRow> {
        #[derive(Clone)]
        enum Pending {
            Edge(ThreadEdge, usize),
            LoadMore(GroupOwner, usize, usize),
        }

        fn push_group(
            pending: &mut Vec<Pending>,
            group: &SiblingGroup,
            owner: GroupOwner,
            depth: usize,
        ) {
            if group.remaining() > 0 {
                pending.push(Pending::LoadMore(owner, depth, group.remaining()));
            }
            pending.extend(
                group
                    .edges
                    .iter()
                    .rev()
                    .cloned()
                    .map(|edge| Pending::Edge(edge, depth)),
            );
        }

        let mut rows = Vec::new();
        let mut pending = Vec::new();
        push_group(&mut pending, &self.roots, GroupOwner::Root, 0);
        while let Some(item) = pending.pop() {
            match item {
                Pending::Edge(ThreadEdge::Node(node), depth) => {
                    rows.push(ThreadRow::Comment { node, depth });
                    if let Some(comment) = self.nodes.get(node)
                        && comment.expanded
                        && let Some(children) = &comment.children
                    {
                        push_group(
                            &mut pending,
                            children,
                            GroupOwner::Node(node),
                            depth.saturating_add(1),
                        );
                    }
                }
                Pending::Edge(ThreadEdge::Marker(marker), depth) => rows.push(ThreadRow::Marker {
                    depth,
                    label: marker.label(),
                }),
                Pending::LoadMore(owner, depth, remaining) => rows.push(ThreadRow::LoadMore {
                    owner,
                    depth,
                    remaining,
                }),
            }
        }
        rows
    }

    pub(crate) fn set_comment(
        &mut self,
        load: CommentLoad,
        result: Result<ContentSnapshot<Comment>, ContentError>,
    ) {
        if load.generation != self.generation {
            return;
        }
        let Some(node) = self.nodes.get_mut(load.node) else {
            return;
        };
        if node.id != load.id {
            return;
        }

        let preserve = load.update == CommentUpdate::Revalidation
            && result.is_err()
            && matches!(
                &node.state,
                ThreadNodeState::Ready(snapshot) if snapshot.usable_after_failure()
            );
        if preserve {
            let ThreadNodeState::Ready(snapshot) = &mut node.state else {
                unreachable!("the preserve guard requires a ready comment")
            };
            snapshot.mark_stale();
            return;
        }

        node.state = match result {
            Ok(snapshot) if snapshot.value.id() == load.id => {
                ThreadNodeState::Ready(Box::new(snapshot))
            }
            Err(ContentError::Retryable) => ThreadNodeState::Retryable,
            Ok(_) | Err(ContentError::Unavailable) => ThreadNodeState::Unavailable,
        };
    }

    pub(crate) fn handle_key(&mut self, key: Key, page_len: usize) -> (bool, Vec<CommentLoad>) {
        let rows = self.rows();
        let last = rows.len().saturating_sub(1);
        let previous = (self.selected, self.offset);
        self.selected = self.selected.min(last);

        let mut structural_changed = false;
        match key {
            Key::Up => self.selected = self.selected.saturating_sub(1),
            Key::Down => self.selected = self.selected.saturating_add(1).min(last),
            Key::PageUp => self.selected = self.selected.saturating_sub(page_len),
            Key::PageDown => self.selected = self.selected.saturating_add(page_len).min(last),
            Key::First => self.selected = 0,
            Key::Last => self.selected = last,
            Key::Left => {
                if let Some(ThreadRow::Comment { node, .. }) = rows.get(self.selected) {
                    if self.nodes[*node].expanded {
                        self.nodes[*node].expanded = false;
                        structural_changed = true;
                    } else if let Some(parent) = self.nodes[*node].parent
                        && let Some(position) = rows.iter().position(
                            |row| matches!(row, ThreadRow::Comment { node, .. } if *node == parent),
                        )
                    {
                        self.selected = position;
                    }
                }
            }
            Key::Right => {
                let before = self.selected_node_signature(&rows);
                let loads = self.expand_selected(&rows);
                let changed = before != self.selected_node_signature(&rows);
                self.keep_visible(page_len);
                return (
                    changed || !loads.is_empty() || previous != (self.selected, self.offset),
                    loads,
                );
            }
            Key::Enter | Key::Toggle => {
                let loads = self.activate_selected(&rows);
                self.keep_visible(page_len);
                return (true, loads);
            }
            Key::Refresh => {
                let loads = self.refresh_selected(&rows);
                return (!loads.is_empty(), loads);
            }
            _ => return (false, Vec::new()),
        }

        self.keep_visible(page_len);
        (
            structural_changed || previous != (self.selected, self.offset),
            Vec::new(),
        )
    }

    fn selected_node_signature(&self, rows: &[ThreadRow]) -> Option<(bool, bool)> {
        let ThreadRow::Comment { node, .. } = rows.get(self.selected)? else {
            return None;
        };
        Some((
            self.nodes[*node].expanded,
            matches!(self.nodes[*node].state, ThreadNodeState::Retryable),
        ))
    }

    fn activate_selected(&mut self, rows: &[ThreadRow]) -> Vec<CommentLoad> {
        match rows.get(self.selected) {
            Some(ThreadRow::Comment { node, .. }) => {
                let node = *node;
                if matches!(self.nodes[node].state, ThreadNodeState::Retryable) {
                    self.nodes[node].state = ThreadNodeState::Loading;
                    return vec![self.comment_load(node, CommentUpdate::Initial)];
                }
                if self.nodes[node].expanded {
                    self.nodes[node].expanded = false;
                    Vec::new()
                } else {
                    self.expand_node(node)
                }
            }
            Some(ThreadRow::LoadMore { owner, .. }) => self.load_group(*owner),
            Some(ThreadRow::Marker { .. }) | None => Vec::new(),
        }
    }

    fn refresh_selected(&mut self, rows: &[ThreadRow]) -> Vec<CommentLoad> {
        let Some(ThreadRow::Comment { node, .. }) = rows.get(self.selected) else {
            return Vec::new();
        };
        let node = *node;
        if matches!(self.nodes[node].state, ThreadNodeState::Loading) {
            return Vec::new();
        }
        vec![self.comment_load(node, CommentUpdate::Revalidation)]
    }

    fn expand_selected(&mut self, rows: &[ThreadRow]) -> Vec<CommentLoad> {
        let Some(ThreadRow::Comment { node, .. }) = rows.get(self.selected) else {
            return Vec::new();
        };
        let node = *node;
        if matches!(self.nodes[node].state, ThreadNodeState::Retryable) {
            self.nodes[node].state = ThreadNodeState::Loading;
            return vec![self.comment_load(node, CommentUpdate::Initial)];
        }
        self.expand_node(node)
    }

    fn expand_node(&mut self, node: usize) -> Vec<CommentLoad> {
        if self.nodes[node].expanded {
            return Vec::new();
        }
        let child_ids = match &self.nodes[node].state {
            ThreadNodeState::Ready(snapshot) => snapshot.value.child_ids().to_vec(),
            ThreadNodeState::Loading
            | ThreadNodeState::Unavailable
            | ThreadNodeState::Retryable => return Vec::new(),
        };
        if child_ids.is_empty() {
            return Vec::new();
        }
        self.nodes[node].expanded = true;
        if self.nodes[node].children.is_none() {
            self.nodes[node].children = Some(SiblingGroup::new(child_ids));
            self.load_group(GroupOwner::Node(node))
        } else {
            Vec::new()
        }
    }

    fn load_group(&mut self, owner: GroupOwner) -> Vec<CommentLoad> {
        let depth = match owner {
            GroupOwner::Root => 0,
            GroupOwner::Node(node) => self.nodes[node].depth.saturating_add(1),
        };
        let (ids, reaches_end) = {
            let group = self.group_mut(owner);
            let start = group.next;
            let end = start
                .saturating_add(COMMENT_BATCH_SIZE)
                .min(group.ids.len());
            group.next = end;
            (group.ids[start..end].to_vec(), end == group.ids.len())
        };
        if ids.is_empty() {
            return Vec::new();
        }

        let mut edges = Vec::new();
        let mut loads = Vec::new();
        if depth >= MAX_THREAD_DEPTH {
            edges.push(ThreadEdge::Marker(ThreadMarker::DepthLimit));
            self.finish_group(owner);
        } else {
            for id in ids {
                if self.is_on_path(owner, id) {
                    edges.push(ThreadEdge::Marker(ThreadMarker::Cycle(id)));
                    continue;
                }
                if self.seen.contains(&id) {
                    edges.push(ThreadEdge::Marker(ThreadMarker::Duplicate(id)));
                    continue;
                }
                if self.nodes.len() >= MAX_THREAD_NODES {
                    edges.push(ThreadEdge::Marker(ThreadMarker::NodeLimit));
                    self.finish_group(owner);
                    break;
                }
                let node = self.nodes.len();
                self.nodes.push(ThreadNode {
                    id,
                    parent: match owner {
                        GroupOwner::Root => None,
                        GroupOwner::Node(parent) => Some(parent),
                    },
                    depth,
                    state: ThreadNodeState::Loading,
                    expanded: false,
                    children: None,
                });
                self.seen.insert(id);
                edges.push(ThreadEdge::Node(node));
                loads.push(self.comment_load(node, CommentUpdate::Initial));
            }
        }
        let has_node_limit = edges
            .iter()
            .any(|edge| matches!(edge, ThreadEdge::Marker(ThreadMarker::NodeLimit)));
        self.group_mut(owner).edges.extend(edges);
        if !reaches_end && self.nodes.len() >= MAX_THREAD_NODES && !has_node_limit {
            self.group_mut(owner)
                .edges
                .push(ThreadEdge::Marker(ThreadMarker::NodeLimit));
            self.finish_group(owner);
        }
        loads
    }

    fn is_on_path(&self, owner: GroupOwner, id: CommentId) -> bool {
        if id == self.entry_id {
            return true;
        }
        let mut current = match owner {
            GroupOwner::Root => None,
            GroupOwner::Node(node) => Some(node),
        };
        while let Some(node) = current {
            if self.nodes[node].id == id {
                return true;
            }
            current = self.nodes[node].parent;
        }
        false
    }

    fn group_mut(&mut self, owner: GroupOwner) -> &mut SiblingGroup {
        match owner {
            GroupOwner::Root => &mut self.roots,
            GroupOwner::Node(node) => self.nodes[node]
                .children
                .as_mut()
                .expect("expanded comment has a child group"),
        }
    }

    fn finish_group(&mut self, owner: GroupOwner) {
        let group = self.group_mut(owner);
        group.next = group.ids.len();
    }

    fn comment_load(&self, node: usize, update: CommentUpdate) -> CommentLoad {
        CommentLoad {
            generation: self.generation,
            node,
            id: self.nodes[node].id,
            update,
        }
    }

    fn keep_visible(&mut self, page_len: usize) {
        let page_len = page_len.max(1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset.saturating_add(page_len) {
            self.offset = self.selected + 1 - page_len;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content::{CommentDetails, ContentSnapshot};

    fn available(id: CommentId, child_ids: Vec<CommentId>) -> ContentSnapshot<Comment> {
        ContentSnapshot::fresh(Comment::Available(CommentDetails {
            id,
            parent: None,
            author: Some(format!("author-{id}")),
            published_at: None,
            text: None,
            child_ids,
            links: Vec::new(),
            omitted_link_count: 0,
        }))
    }

    #[test]
    fn sibling_batches_are_explicit_and_duplicates_are_not_fetched_twice() {
        let mut ids = (1..=21).collect::<Vec<_>>();
        ids.push(1);
        let (mut thread, first) = CommentThread::open(10_000, ids, 7);
        assert_eq!(first.len(), COMMENT_BATCH_SIZE);
        assert_eq!(thread.nodes.len(), COMMENT_BATCH_SIZE);
        assert!(matches!(
            thread.rows().last(),
            Some(ThreadRow::LoadMore { remaining: 2, .. })
        ));

        let second = thread.load_group(GroupOwner::Root);
        assert_eq!(second.len(), 1);
        assert_eq!(thread.nodes.len(), 21);
        assert!(thread.rows().iter().any(|row| matches!(
            row,
            ThreadRow::Marker { label, .. } if label == "[duplicate comment 1 omitted]"
        )));
    }

    #[test]
    fn depth_and_materialized_node_limits_end_with_visible_markers() {
        let (mut deep, mut loads) = CommentThread::open(10_000, vec![1], 1);
        for id in 1..=MAX_THREAD_DEPTH {
            let load = loads.pop().expect("one comment at each permitted depth");
            deep.set_comment(load, Ok(available(id as u64, vec![id as u64 + 1])));
            let node = load.node;
            loads = deep.expand_node(node);
        }
        assert!(loads.is_empty());
        assert_eq!(deep.nodes.len(), MAX_THREAD_DEPTH);
        assert!(deep.rows().iter().any(|row| matches!(
            row,
            ThreadRow::Marker { label, .. }
                if label == "[thread depth limit of 64 reached]"
        )));

        let ids = (1..=u64::try_from(MAX_THREAD_NODES + 1).unwrap()).collect();
        let (mut wide, first) = CommentThread::open(20_000, ids, 2);
        let mut scheduled = first.len();
        while wide.nodes.len() < MAX_THREAD_NODES {
            scheduled += wide.load_group(GroupOwner::Root).len();
        }
        assert_eq!(scheduled, MAX_THREAD_NODES);
        assert!(wide.load_group(GroupOwner::Root).is_empty());
        assert_eq!(wide.nodes.len(), MAX_THREAD_NODES);
        assert_eq!(
            wide.rows()
                .iter()
                .filter(|row| matches!(
                    row,
                    ThreadRow::Marker { label, .. }
                        if label == "[thread node limit of 1000 reached]"
                ))
                .count(),
            1
        );
    }
}
