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
    refreshing: bool,
    expanded: bool,
    children: Option<SiblingGroup>,
}

impl ThreadNode {
    pub(crate) fn id(&self) -> CommentId {
        self.id
    }

    pub(crate) fn parent(&self) -> Option<usize> {
        self.parent
    }

    pub(crate) fn state(&self) -> &ThreadNodeState {
        &self.state
    }

    pub(crate) fn expanded(&self) -> bool {
        self.expanded
    }

    pub(crate) fn refreshing(&self) -> bool {
        self.refreshing
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
        guides: Vec<bool>,
        is_last: bool,
    },
    LoadMore {
        owner: GroupOwner,
        depth: usize,
        remaining: usize,
        guides: Vec<bool>,
        is_last: bool,
    },
    Marker {
        depth: usize,
        label: String,
        guides: Vec<bool>,
        is_last: bool,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct StructuralActions {
    pub(crate) parent: bool,
    pub(crate) next: bool,
    pub(crate) previous: bool,
    pub(crate) child: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CollapsedComments {
    pub(crate) count: usize,
    pub(crate) exact: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct CommentThread {
    entry_id: FeedEntryId,
    generation: u64,
    roots: SiblingGroup,
    nodes: Vec<ThreadNode>,
    seen: HashSet<CommentId>,
    selected_node: Option<usize>,
    selected_row: usize,
    selection_engaged: bool,
    reveal_selection: bool,
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
            selected_node: None,
            selected_row: 0,
            selection_engaged: false,
            reveal_selection: false,
        };
        let loads = thread.load_group(GroupOwner::Root);
        thread.selected_node = loads.first().map(|load| load.node);
        (thread, loads)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn position(&self) -> usize {
        self.selected_position(&self.rows())
    }

    pub(crate) fn selected_node(&self) -> Option<usize> {
        self.selected_node
    }

    pub(crate) fn comment_selection_engaged(&self) -> bool {
        self.selection_engaged
    }

    pub(crate) fn node(&self, index: usize) -> Option<&ThreadNode> {
        self.nodes.get(index)
    }

    #[cfg(test)]
    pub(crate) fn selected_comment_id(&self) -> Option<CommentId> {
        if !self.selection_engaged {
            return None;
        }
        self.selected_node
            .and_then(|node| self.nodes.get(node))
            .map(ThreadNode::id)
    }

    pub(crate) fn structural_actions(&self, node: usize) -> StructuralActions {
        let Some(comment) = self.nodes.get(node) else {
            return StructuralActions::default();
        };
        let owner = comment.parent.map_or(GroupOwner::Root, GroupOwner::Node);
        let siblings = self.sibling_nodes(owner);
        let position = siblings.iter().position(|&sibling| sibling == node);
        StructuralActions {
            parent: comment.parent.is_some(),
            next: position.is_some_and(|position| position + 1 < siblings.len())
                || self.group(owner).remaining() > 0,
            previous: position.is_some_and(|position| position > 0),
            child: comment.has_children(),
        }
    }

    pub(crate) fn collapsed_comments(&self, node: usize) -> CollapsedComments {
        fn visit(
            thread: &CommentThread,
            node: usize,
            seen: &mut HashSet<CommentId>,
            summary: &mut CollapsedComments,
        ) {
            let child_ids = match thread.nodes.get(node).map(ThreadNode::state) {
                Some(ThreadNodeState::Ready(snapshot)) => snapshot.value.child_ids().to_vec(),
                Some(
                    ThreadNodeState::Loading
                    | ThreadNodeState::Unavailable
                    | ThreadNodeState::Retryable,
                )
                | None => {
                    summary.exact = false;
                    return;
                }
            };
            let materialized = thread.nodes[node]
                .children
                .as_ref()
                .map(|group| {
                    group
                        .edges
                        .iter()
                        .filter_map(|edge| match edge {
                            ThreadEdge::Node(child) => Some((thread.nodes[*child].id(), *child)),
                            ThreadEdge::Marker(_) => None,
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();

            for id in child_ids {
                if !seen.insert(id) {
                    continue;
                }
                summary.count = summary.count.saturating_add(1);
                if let Some((_, child)) = materialized.iter().find(|(child_id, _)| *child_id == id)
                {
                    visit(thread, *child, seen, summary);
                } else {
                    summary.exact = false;
                }
            }
        }

        let mut summary = CollapsedComments {
            count: 0,
            exact: true,
        };
        let mut seen = HashSet::new();
        if let Some(comment) = self.nodes.get(node) {
            seen.insert(comment.id());
            visit(self, node, &mut seen, &mut summary);
        } else {
            summary.exact = false;
        }
        summary
    }

    pub(crate) fn rows(&self) -> Vec<ThreadRow> {
        fn push_group(
            rows: &mut Vec<ThreadRow>,
            nodes: &[ThreadNode],
            group: &SiblingGroup,
            owner: GroupOwner,
            depth: usize,
            guides: &[bool],
        ) {
            let has_load_more = group.remaining() > 0;
            let visible_count = group.edges.len() + usize::from(has_load_more);
            for (position, edge) in group.edges.iter().enumerate() {
                let is_last = position + 1 == visible_count;
                match edge {
                    ThreadEdge::Node(node) => {
                        rows.push(ThreadRow::Comment {
                            node: *node,
                            depth,
                            guides: guides.to_vec(),
                            is_last,
                        });
                        if let Some(comment) = nodes.get(*node)
                            && comment.expanded
                            && let Some(children) = &comment.children
                        {
                            let mut child_guides = guides.to_vec();
                            child_guides.push(!is_last);
                            push_group(
                                rows,
                                nodes,
                                children,
                                GroupOwner::Node(*node),
                                depth.saturating_add(1),
                                &child_guides,
                            );
                        }
                    }
                    ThreadEdge::Marker(marker) => rows.push(ThreadRow::Marker {
                        depth,
                        label: marker.label(),
                        guides: guides.to_vec(),
                        is_last,
                    }),
                }
            }
            if has_load_more {
                rows.push(ThreadRow::LoadMore {
                    owner,
                    depth,
                    remaining: group.remaining(),
                    guides: guides.to_vec(),
                    is_last: true,
                });
            }
        }

        let mut rows = Vec::new();
        push_group(
            &mut rows,
            &self.nodes,
            &self.roots,
            GroupOwner::Root,
            0,
            &[],
        );
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

        let starts_revalidation = load.update == CommentUpdate::Initial
            && result
                .as_ref()
                .is_ok_and(|snapshot| snapshot.freshness.is_stale());
        node.refreshing = starts_revalidation;

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
        } else {
            node.state = match result {
                Ok(snapshot) if snapshot.value.id() == load.id => {
                    ThreadNodeState::Ready(Box::new(snapshot))
                }
                Err(ContentError::Retryable) => ThreadNodeState::Retryable,
                Ok(_) | Err(ContentError::Unavailable) => ThreadNodeState::Unavailable,
            };
        }
        if self.selection_engaged {
            self.reveal_selection = true;
        }
    }

    pub(crate) fn materialize_near(&mut self, nodes: &[usize]) -> (bool, Vec<CommentLoad>) {
        let mut materialized = false;
        let mut loads = Vec::new();
        let mut unique = HashSet::new();
        for &node in nodes {
            if unique.insert(node) && self.nodes.get(node).is_some_and(ThreadNode::expanded) {
                let (changed, child_loads) = self.ensure_children(node);
                materialized |= changed;
                loads.extend(child_loads);
            }
        }
        (materialized, loads)
    }

    pub(crate) fn request_reveal(&mut self) {
        if self.selected_node.is_some() {
            self.reveal_selection = true;
        }
    }

    pub(crate) fn take_reveal_request(&mut self) -> bool {
        std::mem::take(&mut self.reveal_selection)
    }

    pub(crate) fn handle_key(&mut self, key: Key) -> (bool, Vec<CommentLoad>) {
        let rows = self.rows();
        let previous_node = self.selected_node;
        let previous_row = self.selected_position(&rows);
        let mut loads = Vec::new();
        let mut changed = false;
        let mut returned_to_entry = false;

        match key {
            Key::Up if !self.selection_engaged => {}
            Key::Up if previous_row == 0 => {
                changed = true;
                returned_to_entry = true;
            }
            Key::Up => changed = self.move_linear(&rows, -1),
            Key::Down if !self.selection_engaged && !rows.is_empty() => changed = true,
            Key::Down => changed = self.move_linear(&rows, 1),
            Key::Left => {
                let Some(node) = self.selected_node else {
                    return (false, loads);
                };
                if self.nodes[node].expanded {
                    self.nodes[node].expanded = false;
                    changed = true;
                } else if let Some(parent) = self.nodes[node].parent {
                    self.select_node(parent);
                    changed = true;
                }
            }
            Key::Right => {
                let Some(node) = self.selected_node else {
                    return (false, loads);
                };
                let (expanded, child_loads) = self.expand_node(node);
                changed = expanded;
                loads.extend(child_loads);
            }
            Key::Toggle => {
                let Some(node) = self.selected_node else {
                    return (false, loads);
                };
                if self.nodes[node].expanded {
                    self.nodes[node].expanded = false;
                    changed = true;
                } else {
                    let (_, child_loads) = self.expand_node(node);
                    loads.extend(child_loads);
                    changed = true;
                }
            }
            Key::CollapseAll => {
                for node in &mut self.nodes {
                    node.expanded = false;
                }
                if let Some(mut selected) = self.selected_node {
                    while let Some(parent) = self.nodes[selected].parent {
                        selected = parent;
                    }
                    self.select_node(selected);
                }
                changed = true;
            }
            Key::ExpandAll => {
                for node in &mut self.nodes {
                    node.expanded = true;
                }
                changed = true;
            }
            Key::NextSibling => {
                let Some(node) = self.selected_node else {
                    return (false, loads);
                };
                let (next, new_loads) = self.next_sibling(node);
                loads.extend(new_loads);
                if let Some(next) = next {
                    self.select_node(next);
                    changed = true;
                }
            }
            Key::PreviousSibling => {
                if let Some(previous) = self
                    .selected_node
                    .and_then(|node| self.previous_sibling(node))
                {
                    self.select_node(previous);
                    changed = true;
                }
            }
            Key::Parent => {
                if let Some(parent) = self
                    .selected_node
                    .and_then(|node| self.nodes.get(node))
                    .and_then(ThreadNode::parent)
                {
                    self.select_node(parent);
                    changed = true;
                }
            }
            Key::FirstChild => {
                let Some(node) = self.selected_node else {
                    return (false, loads);
                };
                let (expanded, child_loads) = self.expand_node(node);
                changed = expanded;
                loads.extend(child_loads);
                if let Some(child) = self.first_child(node) {
                    self.select_node(child);
                    changed = true;
                }
            }
            Key::Enter => {
                let selected = self.selected_position(&rows);
                match rows.get(selected) {
                    Some(ThreadRow::Comment { node, .. })
                        if matches!(self.nodes[*node].state, ThreadNodeState::Retryable) =>
                    {
                        self.nodes[*node].state = ThreadNodeState::Loading;
                        loads.push(self.comment_load(*node, CommentUpdate::Initial));
                        changed = true;
                    }
                    Some(ThreadRow::LoadMore { owner, .. }) => {
                        loads.extend(self.load_group(*owner));
                        changed = true;
                    }
                    Some(ThreadRow::Comment { .. }) | Some(ThreadRow::Marker { .. }) | None => {}
                }
            }
            Key::Refresh => {
                let Some(node) = self.selected_node else {
                    return (false, loads);
                };
                if !self.nodes[node].refreshing
                    && !matches!(self.nodes[node].state, ThreadNodeState::Loading)
                {
                    self.nodes[node].refreshing = true;
                    loads.push(self.comment_load(node, CommentUpdate::Revalidation));
                    changed = true;
                }
            }
            _ => return (false, loads),
        }

        if changed || previous_node != self.selected_node || previous_row != self.position() {
            self.selection_engaged = !returned_to_entry;
            self.reveal_selection = !returned_to_entry;
        }
        (changed || !loads.is_empty(), loads)
    }

    fn move_linear(&mut self, rows: &[ThreadRow], direction: isize) -> bool {
        if rows.is_empty() {
            return false;
        }
        let current = self.selected_position(rows);
        let next = if direction > 0 {
            current.saturating_add(1).min(rows.len().saturating_sub(1))
        } else {
            current.saturating_sub(1)
        };
        if next == current {
            return false;
        }
        self.set_selection_from_row(next, rows);
        true
    }

    fn select_node(&mut self, node: usize) {
        self.selected_node = Some(node);
        self.selection_engaged = true;
        self.reveal_selection = true;
    }

    fn set_selection_from_row(&mut self, position: usize, rows: &[ThreadRow]) {
        self.selected_row = position;
        self.selected_node = match rows.get(position) {
            Some(ThreadRow::Comment { node, .. }) => Some(*node),
            Some(ThreadRow::LoadMore { .. }) | Some(ThreadRow::Marker { .. }) | None => None,
        };
    }

    fn selected_position(&self, rows: &[ThreadRow]) -> usize {
        self.selected_node
            .and_then(|selected| {
                rows.iter().position(
                    |row| matches!(row, ThreadRow::Comment { node, .. } if *node == selected),
                )
            })
            .unwrap_or(self.selected_row)
            .min(rows.len().saturating_sub(1))
    }

    fn next_sibling(&mut self, node: usize) -> (Option<usize>, Vec<CommentLoad>) {
        let owner = self.nodes[node]
            .parent
            .map_or(GroupOwner::Root, GroupOwner::Node);
        let mut loads = Vec::new();
        loop {
            let siblings = self.sibling_nodes(owner);
            let Some(position) = siblings.iter().position(|&sibling| sibling == node) else {
                return (None, loads);
            };
            if let Some(next) = siblings.get(position + 1) {
                return (Some(*next), loads);
            }
            if self.group(owner).remaining() == 0 {
                return (None, loads);
            }
            let loaded = self.load_group(owner);
            if loaded.is_empty() && self.group(owner).remaining() == 0 {
                return (None, loads);
            }
            loads.extend(loaded);
        }
    }

    fn previous_sibling(&self, node: usize) -> Option<usize> {
        let owner = self.nodes[node]
            .parent
            .map_or(GroupOwner::Root, GroupOwner::Node);
        let siblings = self.sibling_nodes(owner);
        let position = siblings.iter().position(|&sibling| sibling == node)?;
        position
            .checked_sub(1)
            .and_then(|position| siblings.get(position).copied())
    }

    fn first_child(&self, node: usize) -> Option<usize> {
        self.nodes[node]
            .children
            .as_ref()?
            .edges
            .iter()
            .find_map(|edge| match edge {
                ThreadEdge::Node(child) => Some(*child),
                ThreadEdge::Marker(_) => None,
            })
    }

    fn sibling_nodes(&self, owner: GroupOwner) -> Vec<usize> {
        self.group(owner)
            .edges
            .iter()
            .filter_map(|edge| match edge {
                ThreadEdge::Node(node) => Some(*node),
                ThreadEdge::Marker(_) => None,
            })
            .collect()
    }

    fn expand_node(&mut self, node: usize) -> (bool, Vec<CommentLoad>) {
        let expanded = !self.nodes[node].expanded;
        self.nodes[node].expanded = true;
        let (materialized, loads) = self.ensure_children(node);
        (expanded || materialized, loads)
    }

    fn ensure_children(&mut self, node: usize) -> (bool, Vec<CommentLoad>) {
        if self.nodes[node].children.is_some() {
            return (false, Vec::new());
        }
        let child_ids = match &self.nodes[node].state {
            ThreadNodeState::Ready(snapshot) => snapshot.value.child_ids().to_vec(),
            ThreadNodeState::Loading
            | ThreadNodeState::Unavailable
            | ThreadNodeState::Retryable => return (false, Vec::new()),
        };
        self.nodes[node].children = Some(SiblingGroup::new(child_ids));
        (true, self.load_group(GroupOwner::Node(node)))
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
                    refreshing: false,
                    expanded: true,
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

    fn group(&self, owner: GroupOwner) -> &SiblingGroup {
        match owner {
            GroupOwner::Root => &self.roots,
            GroupOwner::Node(node) => self.nodes[node]
                .children
                .as_ref()
                .expect("materialized comment has a child group"),
        }
    }

    fn group_mut(&mut self, owner: GroupOwner) -> &mut SiblingGroup {
        match owner {
            GroupOwner::Root => &mut self.roots,
            GroupOwner::Node(node) => self.nodes[node]
                .children
                .as_mut()
                .expect("materialized comment has a child group"),
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
    fn late_comment_results_cannot_mutate_another_thread_generation() {
        let (mut thread, loads) = CommentThread::open(10_000, vec![1], 2);
        thread.set_comment(
            CommentLoad {
                generation: 1,
                ..loads[0]
            },
            Ok(available(1, Vec::new())),
        );
        assert!(matches!(
            thread.node(loads[0].node).map(ThreadNode::state),
            Some(ThreadNodeState::Loading)
        ));

        thread.set_comment(loads[0], Ok(available(1, Vec::new())));
        assert!(matches!(
            thread.node(loads[0].node).map(ThreadNode::state),
            Some(ThreadNodeState::Ready(_))
        ));
    }

    #[test]
    fn repeated_comment_refreshes_are_coalesced_until_the_result_arrives() {
        let (mut thread, loads) = CommentThread::open(10_000, vec![1], 1);
        thread.set_comment(loads[0], Ok(available(1, Vec::new())));

        let (_, first_refresh) = thread.handle_key(Key::Refresh);
        let (_, duplicate_refresh) = thread.handle_key(Key::Refresh);
        assert_eq!(first_refresh.len(), 1);
        assert!(duplicate_refresh.is_empty());

        thread.set_comment(first_refresh[0], Ok(available(1, Vec::new())));
        let (_, next_refresh) = thread.handle_key(Key::Refresh);
        assert_eq!(next_refresh.len(), 1);
    }

    #[test]
    fn selection_stays_on_comment_identity_when_expansion_inserts_rows() {
        let (mut thread, roots) = CommentThread::open(10_000, vec![1, 2], 1);
        thread.set_comment(roots[0], Ok(available(1, vec![3])));
        thread.set_comment(roots[1], Ok(available(2, Vec::new())));
        thread.handle_key(Key::Down);
        assert_eq!(thread.selected_comment_id(), Some(1));
        thread.handle_key(Key::Down);
        assert_eq!(thread.selected_comment_id(), Some(2));

        let (_, children) = thread.materialize_near(&[roots[0].node]);
        thread.set_comment(children[0], Ok(available(3, Vec::new())));

        assert_eq!(thread.selected_comment_id(), Some(2));
        assert_eq!(thread.position(), 2);
    }

    #[test]
    fn up_from_the_first_comment_returns_to_entry_context() {
        let (mut thread, roots) = CommentThread::open(10_000, vec![1, 2], 1);
        thread.set_comment(roots[0], Ok(available(1, Vec::new())));
        thread.set_comment(roots[1], Ok(available(2, Vec::new())));

        assert!(!thread.comment_selection_engaged());
        thread.handle_key(Key::Down);
        assert_eq!(thread.selected_comment_id(), Some(1));
        thread.handle_key(Key::Down);
        assert_eq!(thread.selected_comment_id(), Some(2));
        thread.handle_key(Key::Up);
        assert_eq!(thread.selected_comment_id(), Some(1));
        thread.handle_key(Key::Up);
        assert!(!thread.comment_selection_engaged());
        assert_eq!(thread.selected_comment_id(), None);
    }

    #[test]
    fn structural_keys_follow_siblings_parent_and_first_child() {
        let (mut thread, roots) = CommentThread::open(10_000, vec![1, 2], 1);
        thread.set_comment(roots[0], Ok(available(1, vec![3, 4])));
        thread.set_comment(roots[1], Ok(available(2, Vec::new())));

        let (_, children) = thread.handle_key(Key::FirstChild);
        assert_eq!(children.len(), 2);
        assert_eq!(thread.selected_comment_id(), Some(3));
        assert!(thread.structural_actions(children[0].node).parent);

        thread.handle_key(Key::NextSibling);
        assert_eq!(thread.selected_comment_id(), Some(4));
        assert!(!thread.structural_actions(children[1].node).next);
        thread.handle_key(Key::PreviousSibling);
        assert_eq!(thread.selected_comment_id(), Some(3));
        thread.handle_key(Key::Parent);
        assert_eq!(thread.selected_comment_id(), Some(1));
        thread.handle_key(Key::NextSibling);
        assert_eq!(thread.selected_comment_id(), Some(2));
    }

    #[test]
    fn collapsed_comment_count_includes_known_descendants() {
        let (mut thread, roots) = CommentThread::open(10_000, vec![1], 1);
        thread.set_comment(roots[0], Ok(available(1, vec![2, 3])));
        assert_eq!(
            thread.collapsed_comments(roots[0].node),
            CollapsedComments {
                count: 2,
                exact: false,
            }
        );

        let (_, children) = thread.materialize_near(&[roots[0].node]);
        thread.set_comment(children[0], Ok(available(2, vec![4])));
        thread.set_comment(children[1], Ok(available(3, Vec::new())));
        let (_, grandchild) = thread.materialize_near(&[children[0].node]);
        thread.set_comment(grandchild[0], Ok(available(4, Vec::new())));

        assert_eq!(
            thread.collapsed_comments(roots[0].node),
            CollapsedComments {
                count: 3,
                exact: true,
            }
        );
    }

    #[test]
    fn depth_and_materialized_node_limits_end_with_visible_markers() {
        let (mut deep, mut loads) = CommentThread::open(10_000, vec![1], 1);
        for id in 1..=MAX_THREAD_DEPTH {
            let load = loads.pop().expect("one comment at each permitted depth");
            deep.set_comment(load, Ok(available(id as u64, vec![id as u64 + 1])));
            loads = deep.expand_node(load.node).1;
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
