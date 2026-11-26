//! Unified Back/Forward navigation history for Studio's LIBRARY section.
//!
//! Stores stable keys (never raw `Library` vector indices) so 5-minute server
//! catalogue refreshes and local rescans never point a history entry at the
//! wrong album or artist, and remembers each entry's vertical scroll offset.

use super::{GroupView, Page};

const MAX_HISTORY: usize = 64;

/// A screen inside Studio's LIBRARY section.
#[derive(Clone, Debug, PartialEq)]
pub enum NavView {
    Explore,
    Albums,
    Recent,
    /// `selected`: normalized artist name (`norm(&artist.name)`).
    Artists { selected: Option<String> },
    Songs,
    Playlists,
    /// Music-server playlist ID.
    Playlist(String),
    /// Stable album identity: `(norm(&album.artist), norm(&album.title))`.
    Album { artist: String, title: String },
    /// Genre, mood, or decade page with its active filter chips.
    Group(GroupView),
    /// Search results for a non-empty query.
    Search(String),
    NowPlaying,
}

impl NavView {
    /// The top-level sidebar page to highlight for this view, if any.
    pub fn sidebar_page(&self) -> Option<Page> {
        match self {
            NavView::Explore | NavView::Recent | NavView::Group(_) => Some(Page::Explore),
            NavView::Albums | NavView::Album { .. } => Some(Page::Albums),
            NavView::Artists { .. } => Some(Page::Artists),
            NavView::Songs => Some(Page::Songs),
            NavView::Playlists | NavView::Playlist(_) => Some(Page::Playlists),
            NavView::Search(_) | NavView::NowPlaying => None,
        }
    }

    pub fn from_page(page: Page) -> Self {
        match page {
            Page::Explore => NavView::Explore,
            Page::Albums => NavView::Albums,
            Page::Artists => NavView::Artists { selected: None },
            Page::Songs => NavView::Songs,
            Page::Playlists => NavView::Playlists,
        }
    }
}

#[derive(Clone, Debug)]
struct Entry {
    view: NavView,
    scroll_y: f32,
}

/// Linear Back/Forward history with per-entry scroll restoration.
#[derive(Clone, Debug)]
pub struct NavHistory {
    entries: Vec<Entry>,
    index: usize,
    restore_scroll: Option<f32>,
}

impl Default for NavHistory {
    fn default() -> Self {
        Self::new(NavView::Explore)
    }
}

impl NavHistory {
    pub fn new(initial: NavView) -> Self {
        Self { entries: vec![Entry { view: initial, scroll_y: 0.0 }], index: 0, restore_scroll: None }
    }

    pub fn current(&self) -> &NavView {
        &self.entries[self.index].view
    }

    pub fn can_back(&self) -> bool {
        self.index > 0
    }

    pub fn can_forward(&self) -> bool {
        self.index + 1 < self.entries.len()
    }

    /// Navigate to `view`, dropping any forward entries. No-op if already there.
    pub fn push(&mut self, view: NavView) {
        if self.entries[self.index].view == view {
            return;
        }
        self.entries.truncate(self.index + 1);
        self.entries.push(Entry { view, scroll_y: 0.0 });
        if self.entries.len() > MAX_HISTORY {
            let drop = self.entries.len() - MAX_HISTORY;
            self.entries.drain(0..drop);
        }
        self.index = self.entries.len() - 1;
        self.restore_scroll = Some(0.0);
    }

    /// Update the current entry in place (e.g. typing in Search or toggling filter chips).
    pub fn replace_current(&mut self, view: NavView) {
        self.entries[self.index].view = view;
    }

    pub fn back(&mut self) -> bool {
        if !self.can_back() {
            return false;
        }
        self.index -= 1;
        self.restore_scroll = Some(self.entries[self.index].scroll_y);
        true
    }

    pub fn forward(&mut self) -> bool {
        if !self.can_forward() {
            return false;
        }
        self.index += 1;
        self.restore_scroll = Some(self.entries[self.index].scroll_y);
        true
    }

    /// Record the current page's vertical scroll offset.
    pub fn set_scroll(&mut self, y: f32) {
        self.entries[self.index].scroll_y = y.max(0.0);
    }

    /// Scroll offset to apply once after navigating, if any.
    pub fn take_restore_scroll(&mut self) -> Option<f32> {
        self.restore_scroll.take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library_ui::{GroupKind, GroupView};

    #[test]
    fn back_and_forward_preserve_scroll() {
        let mut h = NavHistory::new(NavView::Explore);
        h.set_scroll(320.0);
        h.push(NavView::Group(GroupView::new(GroupKind::Genre, "rock")));
        assert_eq!(h.take_restore_scroll(), Some(0.0));
        h.set_scroll(140.0);
        h.push(NavView::Album { artist: "radiohead".into(), title: "ok computer".into() });
        assert_eq!(h.take_restore_scroll(), Some(0.0));

        assert!(h.can_back());
        assert!(!h.can_forward());

        assert!(h.back());
        assert_eq!(h.current(), &NavView::Group(GroupView::new(GroupKind::Genre, "rock")));
        assert_eq!(h.take_restore_scroll(), Some(140.0));
        assert!(h.can_forward());

        assert!(h.back());
        assert_eq!(h.current(), &NavView::Explore);
        assert_eq!(h.take_restore_scroll(), Some(320.0));
        assert!(!h.can_back());

        assert!(h.forward());
        assert_eq!(h.current(), &NavView::Group(GroupView::new(GroupKind::Genre, "rock")));
        assert_eq!(h.take_restore_scroll(), Some(140.0));
    }

    #[test]
    fn push_after_back_truncates_forward_branch() {
        let mut h = NavHistory::new(NavView::Explore);
        h.push(NavView::Albums);
        h.push(NavView::Songs);
        assert!(h.back());
        assert_eq!(h.current(), &NavView::Albums);
        h.push(NavView::Playlists);
        assert!(!h.can_forward());
        assert!(h.back());
        assert_eq!(h.current(), &NavView::Albums);
    }

    #[test]
    fn push_same_view_is_noop() {
        let mut h = NavHistory::new(NavView::Explore);
        h.set_scroll(99.0);
        h.push(NavView::Explore);
        assert!(!h.can_back());
        assert_eq!(h.take_restore_scroll(), None);
    }

    #[test]
    fn replace_current_keeps_history_length() {
        let mut h = NavHistory::new(NavView::Explore);
        h.push(NavView::Search("ra".into()));
        h.replace_current(NavView::Search("radiohead".into()));
        assert!(h.back());
        assert_eq!(h.current(), &NavView::Explore);
        assert!(h.forward());
        assert_eq!(h.current(), &NavView::Search("radiohead".into()));
    }
}
