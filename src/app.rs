//! App state and transitions for chronox. All state changes go through
//! `App::apply`. Timeline parsing, navigation, and styled rendering are
//! delegated to the `sessionx` crate; this module is the shell around them.

use ratatui::layout::Rect;
use ratatui::text::Line;
use std::path::PathBuf;

use crate::commits::CommitIndex;
use crate::render::change_detail_lines_styled;
use sessionx::extract::{load_full_change, resolve_line_in_file, session_files};
use sessionx::nav::nav;
use sessionx::{
    ChangeEvent, ChangeSource, ChangeTool, NavAction, NavKey, SideRow, Timeline,
    change_detail_side_by_side, lang_for_path,
};

/// Default columns for the left (list) pane.
pub const DEFAULT_LIST_WIDTH: u16 = 32;
/// Minimum columns the list pane may shrink to.
pub const MIN_LIST: u16 = 16;
/// Minimum columns the diff pane must keep.
pub const MIN_DIFF: u16 = 24;

/// Which pane currently receives ↑/↓.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Diff,
}

/// Which rendering the diff pane uses. Side-by-side is the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffView {
    SideBySide,
    Block,
}

/// Every state change is expressed as one of these and applied via `App::apply`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    Quit,
    Nav(NavKey),
    ToggleFocus,
    ScrollDiff(i32),
    /// Flip the diff pane between side-by-side and block rendering.
    ToggleDiffView,
    NudgeSplit(i32),
    StartResize,
    Resize(u16),
    EndResize,
    /// Open the selected change's file in `$EDITOR`. Side-effecting: handled by
    /// the run loop (which owns the terminal), not by `App::apply`.
    OpenInEditor,
    Tick,
    None,
}

/// One file's worth of changes, newest-first, with rolled-up line counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileGroup {
    pub file: PathBuf,
    pub event_idxs: Vec<usize>, // newest-first, into App.events
    pub add: u32,
    pub del: u32,
    pub is_new: bool, // single Write -> " new" tag
}

/// A row in the rendered list: a file header (always shown) or an edit row
/// (shown only under the active/expanded file).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleRow {
    Header { group: usize }, // index into `groups`
    Edit { event: usize },   // index into App.events
}

/// Group `events` by `file_path`, preserving first-seen order. Because the
/// timeline is newest-first, this yields most-recently-touched file first with
/// edits newest-first inside each group. `counts[i]` is event `i`'s (add, del).
fn build_groups(events: &[ChangeEvent], counts: &[(u32, u32)]) -> Vec<FileGroup> {
    let mut groups: Vec<FileGroup> = Vec::new();
    for (i, ev) in events.iter().enumerate() {
        let (a, d) = counts.get(i).copied().unwrap_or((0, 0));
        match groups.iter_mut().find(|g| g.file == ev.file_path) {
            Some(g) => {
                g.event_idxs.push(i);
                g.add += a;
                g.del += d;
                g.is_new = false; // more than one change -> not a fresh new file
            }
            None => groups.push(FileGroup {
                file: ev.file_path.clone(),
                event_idxs: vec![i],
                add: a,
                del: d,
                is_new: ev.tool == ChangeTool::Write,
            }),
        }
    }
    groups
}

/// Flatten groups into the visible-row sequence: every header in order, with
/// the active group's edit rows inserted directly under its header.
fn build_visible(groups: &[FileGroup], active: usize) -> Vec<VisibleRow> {
    let mut out = Vec::new();
    for (gi, g) in groups.iter().enumerate() {
        out.push(VisibleRow::Header { group: gi });
        if gi == active {
            for &event in &g.event_idxs {
                out.push(VisibleRow::Edit { event });
            }
        }
    }
    out
}

/// What the cursor is "on", independent of row indices, so selection survives a
/// rebuild that expands/collapses rows.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SelTarget {
    File(PathBuf),
    Edit(ChangeSource),
}

pub struct App {
    pub worktree: PathBuf,
    timeline: Timeline,
    /// Changes reconstructed from commits the agent made (shell edits).
    commits: CommitIndex,
    events: Vec<ChangeEvent>,
    groups: Vec<FileGroup>,
    visible: Vec<VisibleRow>,
    pub active_group: usize,
    // `ChangeSource` is `Eq` but not `Hash`, so we memoize in an association
    // list rather than a `HashMap`. Lookups stay keyed by `ChangeSource`.
    counts: Vec<(ChangeSource, (u32, u32))>,
    pub spinner_frame: usize,
    pub selected: usize,
    pub focus: Focus,
    pub diff_scroll: usize,
    pub list_scroll: usize,
    pub list_width: u16,
    pub resizing: bool,
    pub last_area: Rect,
    pub last_visible_rows: usize,
    diff_cache: Option<(ChangeSource, Vec<Line<'static>>)>,
    pub diff_view: DiffView,
    side_cache: Option<(ChangeSource, Vec<SideRow>)>,
    /// Transient one-line message for the footer (e.g. an editor-launch error),
    /// dismissed on the next keypress.
    status: Option<String>,
    pub should_quit: bool,
}

/// Build the full, un-clipped styled diff for one change. Re-reads the session
/// log for fidelity, falling back to the bounded `detail` when the log is
/// unavailable.
fn build_diff_lines(ev: &ChangeEvent) -> Vec<Line<'static>> {
    let detail = load_full_change(ev).unwrap_or_else(|| ev.detail.clone());
    let base = resolve_line_in_file(&ev.file_path, &detail);
    change_detail_lines_styled(&detail, base, lang_for_path(&ev.file_path))
}

/// Build the full, un-clipped side-by-side rows for one change. Same re-read +
/// base-line resolution as `build_diff_lines`.
fn build_side_rows(ev: &ChangeEvent) -> Vec<SideRow> {
    let detail = load_full_change(ev).unwrap_or_else(|| ev.detail.clone());
    let base = resolve_line_in_file(&ev.file_path, &detail);
    change_detail_side_by_side(&detail, base, lang_for_path(&ev.file_path))
}

impl App {
    /// Construct without touching the filesystem. `new` wraps this and refreshes.
    pub(crate) fn bare(worktree: PathBuf) -> Self {
        App {
            worktree,
            timeline: Timeline::default(),
            commits: CommitIndex::default(),
            events: Vec::new(),
            groups: Vec::new(),
            visible: Vec::new(),
            active_group: 0,
            counts: Vec::new(),
            spinner_frame: 0,
            selected: 0,
            focus: Focus::List,
            diff_scroll: 0,
            list_scroll: 0,
            list_width: DEFAULT_LIST_WIDTH,
            resizing: false,
            last_area: Rect::default(),
            last_visible_rows: 0,
            diff_cache: None,
            diff_view: DiffView::SideBySide,
            side_cache: None,
            status: None,
            should_quit: false,
        }
    }

    pub fn events(&self) -> &[ChangeEvent] {
        &self.events
    }

    /// Memoized (add, del) for event `idx`, computed from its bounded detail.
    fn ensure_count(&mut self, idx: usize) {
        if let Some(ev) = self.events.get(idx) {
            let src = ev.source.clone();
            if !self.counts.iter().any(|(s, _)| s == &src) {
                let c = crate::render::change_counts(&ev.detail);
                self.counts.push((src, c));
            }
        }
    }

    /// Read cached counts for event `idx` (0,0 if absent — should not happen
    /// after `rebuild`, which populates every event).
    pub fn event_counts(&self, idx: usize) -> (u32, u32) {
        self.events
            .get(idx)
            .and_then(|ev| {
                self.counts
                    .iter()
                    .find(|(s, _)| s == &ev.source)
                    .map(|(_, c)| *c)
            })
            .unwrap_or((0, 0))
    }

    /// The semantic target currently under the cursor, read from current state.
    fn current_target(&self) -> Option<SelTarget> {
        match self.visible.get(self.selected)? {
            VisibleRow::Header { group } => {
                Some(SelTarget::File(self.groups.get(*group)?.file.clone()))
            }
            VisibleRow::Edit { event } => {
                Some(SelTarget::Edit(self.events.get(*event)?.source.clone()))
            }
        }
    }

    /// Which group a target belongs to (0 if not found / no target).
    fn group_for_target(&self, target: &Option<SelTarget>) -> usize {
        match target {
            Some(SelTarget::File(p)) => self.groups.iter().position(|g| &g.file == p).unwrap_or(0),
            Some(SelTarget::Edit(src)) => self
                .groups
                .iter()
                .position(|g| {
                    g.event_idxs
                        .iter()
                        .any(|&i| self.events.get(i).map(|e| &e.source) == Some(src))
                })
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Index in `visible` matching the target, if present.
    fn locate(&self, target: &Option<SelTarget>) -> Option<usize> {
        let target = target.as_ref()?;
        self.visible.iter().position(|row| match (row, target) {
            (VisibleRow::Header { group }, SelTarget::File(p)) => {
                self.groups.get(*group).map(|g| &g.file) == Some(p)
            }
            (VisibleRow::Edit { event }, SelTarget::Edit(src)) => {
                self.events.get(*event).map(|e| &e.source) == Some(src)
            }
            _ => false,
        })
    }

    /// Re-assign events and rebuild, repinning the cursor onto the same semantic
    /// target it was on *before* the swap. Use this whenever the event list is
    /// replaced — `current_target` resolves `Edit` targets against `self.events`,
    /// so the target must be read while `events`/`visible` are still consistent.
    fn set_events_and_rebuild(&mut self, events: Vec<ChangeEvent>) {
        let target = self.current_target();
        self.events = events;
        self.rebuild_with_target(target);
    }

    /// Rebuild groups + visible from the current events, keeping the cursor on
    /// the same semantic target (accordion: the target's file is active).
    fn rebuild(&mut self) {
        let target = self.current_target();
        self.rebuild_with_target(target);
    }

    /// Rebuild against an explicitly-captured target (see `set_events_and_rebuild`).
    fn rebuild_with_target(&mut self, target: Option<SelTarget>) {
        // Populate the count cache for every event (memoized; cheap after first).
        for i in 0..self.events.len() {
            self.ensure_count(i);
        }
        let counts: Vec<(u32, u32)> = (0..self.events.len())
            .map(|i| self.event_counts(i))
            .collect();
        self.groups = build_groups(&self.events, &counts);
        self.active_group = self.group_for_target(&target);
        self.visible = build_visible(&self.groups, self.active_group);
        self.selected = self
            .locate(&target)
            .unwrap_or_else(|| self.selected.min(self.visible.len().saturating_sub(1)));
    }

    /// Event index the selected row resolves to: an edit's own event, or a
    /// header's group's newest event.
    pub fn selected_event_idx(&self) -> Option<usize> {
        match self.visible.get(self.selected)? {
            VisibleRow::Edit { event } => Some(*event),
            VisibleRow::Header { group } => self.groups.get(*group)?.event_idxs.first().copied(),
        }
    }

    pub fn groups(&self) -> &[FileGroup] {
        &self.groups
    }

    pub fn visible(&self) -> &[VisibleRow] {
        &self.visible
    }

    /// Session line-count totals across all groups.
    pub fn session_totals(&self) -> (u32, u32) {
        self.groups
            .iter()
            .fold((0, 0), |(a, d), g| (a + g.add, d + g.del))
    }

    pub fn selected_event(&self) -> Option<&ChangeEvent> {
        self.events.get(self.selected_event_idx()?)
    }

    /// Absolute path and 1-based line of the current selection, for handing to
    /// an external editor. Reuses the same full-change + line-resolution path
    /// the diff view uses, so the editor lands on the line the diff shows.
    /// `resolve_line_in_file` returns 1 when the file is unreadable, so the
    /// line is always >= 1.
    pub fn selected_path_and_line(&self) -> Option<(PathBuf, u32)> {
        let ev = self.events.get(self.selected_event_idx()?)?;
        let detail = load_full_change(ev).unwrap_or_else(|| ev.detail.clone());
        let line = resolve_line_in_file(&ev.file_path, &detail);
        Some((ev.file_path.clone(), line))
    }

    /// The transient footer message, if any.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Set the transient footer message (shown until the next keypress).
    pub fn set_status(&mut self, msg: String) {
        self.status = Some(msg);
    }

    /// Clear the transient footer message.
    pub fn clear_status(&mut self) {
        self.status = None;
    }
}

impl App {
    /// Test seam — crate-visible so `ui.rs` and `input.rs` tests can seed events
    /// without touching the filesystem.
    #[cfg(test)]
    pub(crate) fn set_events_for_test_pub(&mut self, events: Vec<ChangeEvent>) {
        self.set_events_and_rebuild(events);
    }

    /// Single entry point for all state changes.
    pub fn apply(&mut self, action: AppAction) {
        match action {
            AppAction::Quit => self.should_quit = true,
            AppAction::Tick => {
                self.spinner_frame = self.spinner_frame.wrapping_add(1);
                self.refresh();
            }
            AppAction::ToggleFocus => {
                self.focus = match self.focus {
                    Focus::List => Focus::Diff,
                    Focus::Diff => Focus::List,
                };
            }
            AppAction::Nav(key) => self.on_nav(key),
            AppAction::ScrollDiff(delta) => self.scroll_diff(delta),
            AppAction::ToggleDiffView => {
                self.diff_view = match self.diff_view {
                    DiffView::SideBySide => DiffView::Block,
                    DiffView::Block => DiffView::SideBySide,
                };
                self.diff_scroll = 0;
            }
            AppAction::NudgeSplit(delta) => {
                let target = (self.list_width as i32 + delta).max(0) as u16;
                self.resize_to(target);
            }
            AppAction::StartResize => self.resizing = true,
            AppAction::Resize(col) => {
                let target = col.saturating_sub(self.last_area.x);
                self.resize_to(target);
            }
            AppAction::EndResize => self.resizing = false,
            // Handled by the run loop, which owns the terminal; no state change.
            AppAction::OpenInEditor => {}
            AppAction::None => {}
        }
    }

    fn on_nav(&mut self, key: NavKey) {
        if self.focus == Focus::Diff {
            match key {
                NavKey::Up => return self.scroll_diff(-1),
                NavKey::Down => return self.scroll_diff(1),
                NavKey::Esc => {
                    self.should_quit = true;
                    return;
                }
                _ => {}
            }
        }
        let (new_sel, act) = nav(self.selected, key, self.visible.len());
        match act {
            NavAction::Exit => self.should_quit = true,
            NavAction::Open(_) => self.focus = Focus::Diff,
            NavAction::None => {}
        }
        if new_sel != self.selected {
            self.selected = new_sel;
            self.diff_scroll = 0;
            self.rebuild(); // re-derive active file + visible rows
        }
    }

    fn scroll_diff(&mut self, delta: i32) {
        let next = self.diff_scroll as i64 + delta as i64;
        self.diff_scroll = next.max(0) as usize;
        // The upper bound is clamped against the rendered diff length at draw time.
    }

    fn resize_to(&mut self, target: u16) {
        let max = self
            .last_area
            .width
            .saturating_sub(MIN_DIFF + 1)
            .max(MIN_LIST);
        self.list_width = target.clamp(MIN_LIST, max);
    }

    /// Re-clamp the split width to the current `last_area` — called each frame so
    /// a terminal resize (or an oversized default on first draw) can't push the
    /// divider off-screen or squeeze the diff pane below its minimum.
    pub fn reclamp_split(&mut self) {
        self.resize_to(self.list_width);
    }
}

impl App {
    /// Construct for `worktree` and load its current timeline.
    pub fn new(worktree: PathBuf) -> Self {
        let mut app = Self::bare(worktree);
        app.refresh();
        app
    }

    /// Re-scan the worktree's session logs, rebuild the merged event list, and
    /// re-pin the cursor to the same change. Cheap to call on a tick — the
    /// sessionx `Timeline` reparses only files whose size/mtime changed, and
    /// `CommitIndex` scans only appended log lines and reuses its result until
    /// one grows (then re-reads the reflog and resolves only new commits).
    fn refresh(&mut self) {
        let files = session_files(&self.worktree);
        self.timeline.refresh(&files);
        let mut events = self.timeline.events().to_vec();
        let committed = self.commits.refresh(&self.worktree, &files, &events);
        if !committed.is_empty() {
            events.extend(committed);
            // Same order as sessionx's Timeline. The sort is stable, so a
            // commit's hunks within one file stay top-to-bottom.
            events.sort_by(|a, b| {
                b.timestamp_ms
                    .cmp(&a.timestamp_ms)
                    .then_with(|| a.file_path.cmp(&b.file_path))
            });
        }
        self.set_events_and_rebuild(events);
    }

    /// Styled diff lines for the current selection, built lazily and cached by
    /// the selected change's `ChangeSource` (robust across refresh + selection
    /// changes).
    pub fn diff_lines(&mut self) -> &[Line<'static>] {
        // Safe to key on ChangeSource: Claude Code session logs are append-only,
        // so a given (file, line, index) is written once and never mutated.
        let idx = self.selected_event_idx();
        let src = idx
            .and_then(|i| self.events.get(i))
            .map(|e| e.source.clone());
        let needs = match (&self.diff_cache, &src) {
            (Some((cached, _)), Some(s)) => cached != s,
            _ => true,
        };
        if needs {
            match src {
                Some(s) => {
                    let lines = idx
                        .and_then(|i| self.events.get(i))
                        .map(build_diff_lines)
                        .unwrap_or_default();
                    self.diff_cache = Some((s, lines));
                }
                None => self.diff_cache = None,
            }
        }
        self.diff_cache
            .as_ref()
            .map(|(_, l)| l.as_slice())
            .unwrap_or(&[])
    }

    /// Styled side-by-side rows for the current selection, cached by the
    /// selected change's `ChangeSource` (mirrors `diff_lines`).
    pub fn diff_side_rows(&mut self) -> &[SideRow] {
        let idx = self.selected_event_idx();
        let src = idx
            .and_then(|i| self.events.get(i))
            .map(|e| e.source.clone());
        let needs = match (&self.side_cache, &src) {
            (Some((cached, _)), Some(s)) => cached != s,
            _ => true,
        };
        if needs {
            match src {
                Some(s) => {
                    let rows = idx
                        .and_then(|i| self.events.get(i))
                        .map(build_side_rows)
                        .unwrap_or_default();
                    self.side_cache = Some((s, rows));
                }
                None => self.side_cache = None,
            }
        }
        self.side_cache
            .as_ref()
            .map(|(_, r)| r.as_slice())
            .unwrap_or(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sessionx::{ChangeDetail, ChangeTool};

    fn write_ev(ts: i64, file: &str, line_index: usize) -> ChangeEvent {
        ChangeEvent {
            timestamp_ms: ts,
            tool: ChangeTool::Write,
            file_path: PathBuf::from(file),
            summary: String::new(),
            detail: ChangeDetail::Write { head: "x".into() },
            source: ChangeSource {
                session_file: PathBuf::from("s.jsonl"),
                line_index,
                index_in_line: 0,
            },
        }
    }

    fn ev(ts: i64, file: &str, line_index: usize) -> ChangeEvent {
        ChangeEvent {
            timestamp_ms: ts,
            tool: ChangeTool::Edit,
            file_path: PathBuf::from(file),
            summary: String::new(),
            detail: ChangeDetail::Edit {
                old: "a".into(),
                new: "b".into(),
            },
            source: ChangeSource {
                session_file: PathBuf::from("s.jsonl"),
                line_index,
                index_in_line: 0,
            },
        }
    }

    #[test]
    fn list_focus_moves_over_visible_rows() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.set_events_for_test_pub(vec![
            ev(3, "/wt/a.rs", 1),
            ev(2, "/wt/b.rs", 2),
            ev(1, "/wt/c.rs", 3),
        ]);
        app.focus = Focus::List;
        // Starts on a.rs header (active); its single edit is event 0.
        assert_eq!(app.selected_event_idx(), Some(0));
        app.apply(AppAction::Nav(NavKey::Down)); // onto a.rs's edit
        assert_eq!(app.selected_event_idx(), Some(0));
        app.apply(AppAction::Nav(NavKey::Bottom)); // last visible row -> c.rs header
        assert_eq!(app.selected_event_idx(), Some(2));
        app.apply(AppAction::Nav(NavKey::Top)); // back to a.rs header
        assert_eq!(app.selected_event_idx(), Some(0));
    }

    #[test]
    fn moving_selection_resets_diff_scroll() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.set_events_for_test_pub(vec![ev(2, "/wt/a.rs", 1), ev(1, "/wt/b.rs", 2)]);
        app.focus = Focus::List;
        app.diff_scroll = 7;
        app.apply(AppAction::Nav(NavKey::Down));
        assert_eq!(app.diff_scroll, 0);
    }

    #[test]
    fn rebuild_keeps_cursor_on_same_edit_when_new_change_prepended() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.set_events_for_test_pub(vec![ev(2, "/wt/a.rs", 1), ev(1, "/wt/b.rs", 2)]);
        app.focus = Focus::List;
        app.apply(AppAction::Nav(NavKey::Down)); // onto a.rs's edit (event 0, source line 1)
        assert_eq!(app.selected_event_idx(), Some(0));
        // A newer change to a different file is prepended; a.rs's edit shifts down.
        app.set_events_for_test_pub(vec![
            ev(3, "/wt/new.rs", 9),
            ev(2, "/wt/a.rs", 1),
            ev(1, "/wt/b.rs", 2),
        ]);
        // Still pinned to the same a.rs change (now event index 1).
        assert_eq!(app.selected_event_idx(), Some(1));
    }

    #[test]
    fn diff_focus_routes_arrows_to_scroll() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.set_events_for_test_pub(vec![ev(1, "/wt/a.rs", 1)]);
        app.focus = Focus::Diff;
        app.diff_scroll = 3;
        app.apply(AppAction::Nav(NavKey::Up));
        assert_eq!(app.diff_scroll, 2);
        assert_eq!(app.selected, 0, "diff focus must not move the list");
        app.apply(AppAction::Nav(NavKey::Down));
        assert_eq!(app.diff_scroll, 3);
    }

    #[test]
    fn scroll_diff_floors_at_zero() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.diff_scroll = 0;
        app.apply(AppAction::ScrollDiff(-5));
        assert_eq!(app.diff_scroll, 0);
        app.apply(AppAction::ScrollDiff(4));
        assert_eq!(app.diff_scroll, 4);
    }

    #[test]
    fn toggle_focus_flips() {
        let mut app = App::bare(PathBuf::from("/wt"));
        assert_eq!(app.focus, Focus::List);
        app.apply(AppAction::ToggleFocus);
        assert_eq!(app.focus, Focus::Diff);
        app.apply(AppAction::ToggleFocus);
        assert_eq!(app.focus, Focus::List);
    }

    #[test]
    fn esc_and_quit_set_should_quit() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.apply(AppAction::Nav(NavKey::Esc));
        assert!(app.should_quit);
        let mut app2 = App::bare(PathBuf::from("/wt"));
        app2.apply(AppAction::Quit);
        assert!(app2.should_quit);
    }

    #[test]
    fn resize_clamps_to_bounds() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.last_area = Rect::new(0, 0, 100, 30); // max = 100 - MIN_DIFF(24) - 1 = 75
        app.apply(AppAction::Resize(5)); // below MIN_LIST
        assert_eq!(app.list_width, MIN_LIST);
        app.apply(AppAction::Resize(90)); // above max
        assert_eq!(app.list_width, 75);
        app.apply(AppAction::Resize(40));
        assert_eq!(app.list_width, 40);
    }

    #[test]
    fn nudge_split_respects_bounds() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.last_area = Rect::new(0, 0, 100, 30);
        app.list_width = MIN_LIST;
        app.apply(AppAction::NudgeSplit(-1));
        assert_eq!(app.list_width, MIN_LIST, "cannot go below MIN_LIST");
        app.apply(AppAction::NudgeSplit(1));
        assert_eq!(app.list_width, MIN_LIST + 1);
    }

    #[test]
    fn resize_flag_transitions() {
        let mut app = App::bare(PathBuf::from("/wt"));
        assert!(!app.resizing);
        app.apply(AppAction::StartResize);
        assert!(app.resizing);
        app.apply(AppAction::EndResize);
        assert!(!app.resizing);
    }

    #[test]
    fn new_on_empty_worktree_has_no_events() {
        let dir = tempfile::TempDir::new().unwrap();
        let app = App::new(dir.path().to_path_buf());
        assert!(app.events().is_empty());
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn diff_lines_falls_back_to_detail_when_log_missing() {
        let mut app = App::bare(PathBuf::from("/wt"));
        // source.session_file does not exist → load_full_change returns None →
        // we fall back to ev.detail (an Edit), which yields at least one diff line.
        app.set_events_for_test_pub(vec![ev(1, "/wt/a.rs", 1)]);
        let lines = app.diff_lines().to_vec();
        assert!(
            !lines.is_empty(),
            "fallback detail must still render a diff"
        );
    }

    #[test]
    fn diff_lines_empty_when_no_events() {
        let mut app = App::bare(PathBuf::from("/wt"));
        assert!(app.diff_lines().is_empty());
    }

    #[test]
    fn selected_path_and_line_returns_path_and_line() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.set_events_for_test_pub(vec![ev(1, "/wt/a.rs", 1)]);
        // The source log is absent, so resolve_line_in_file falls back to 1.
        let (path, line) = app.selected_path_and_line().expect("a selection exists");
        assert_eq!(path, PathBuf::from("/wt/a.rs"));
        assert_eq!(line, 1);
    }

    #[test]
    fn selected_path_and_line_none_when_no_events() {
        let app = App::bare(PathBuf::from("/wt"));
        assert!(app.selected_path_and_line().is_none());
    }

    #[test]
    fn status_sets_and_clears() {
        let mut app = App::bare(PathBuf::from("/wt"));
        assert_eq!(app.status(), None);
        app.set_status("nope".into());
        assert_eq!(app.status(), Some("nope"));
        app.clear_status();
        assert_eq!(app.status(), None);
    }

    #[test]
    fn default_diff_view_is_side_by_side() {
        let app = App::bare(PathBuf::from("/wt"));
        assert_eq!(app.diff_view, DiffView::SideBySide);
    }

    #[test]
    fn toggle_diff_view_flips_and_resets_scroll() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.diff_scroll = 9;
        app.apply(AppAction::ToggleDiffView);
        assert_eq!(app.diff_view, DiffView::Block);
        assert_eq!(app.diff_scroll, 0, "toggling resets the diff scroll");
        app.apply(AppAction::ToggleDiffView);
        assert_eq!(app.diff_view, DiffView::SideBySide);
    }

    #[test]
    fn diff_side_rows_falls_back_to_detail_when_log_missing() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.set_events_for_test_pub(vec![ev(1, "/wt/a.rs", 1)]);
        // ev() is an Edit { old: "a", new: "b" } -> one zipped replace row.
        let rows = app.diff_side_rows().to_vec();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].left.is_some() && rows[0].right.is_some());
    }

    #[test]
    fn diff_side_rows_empty_when_no_events() {
        let mut app = App::bare(PathBuf::from("/wt"));
        assert!(app.diff_side_rows().is_empty());
    }

    #[test]
    fn build_groups_orders_by_first_seen_and_rolls_up() {
        // newest-first input (as sessionx hands us): a, a, b, write-c
        let events = vec![
            ev(4, "/wt/a.rs", 1),
            ev(3, "/wt/a.rs", 2),
            ev(2, "/wt/b.rs", 3),
            write_ev(1, "/wt/c.rs", 4),
        ];
        // per-event (add, del)
        let counts = vec![(10, 1), (4, 0), (2, 2), (58, 0)];
        let groups = build_groups(&events, &counts);

        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].file, PathBuf::from("/wt/a.rs"));
        assert_eq!(groups[0].event_idxs, vec![0, 1]);
        assert_eq!((groups[0].add, groups[0].del), (14, 1));
        assert!(!groups[0].is_new);

        assert_eq!(groups[1].file, PathBuf::from("/wt/b.rs"));
        assert_eq!((groups[1].add, groups[1].del), (2, 2));

        assert_eq!(groups[2].file, PathBuf::from("/wt/c.rs"));
        assert!(groups[2].is_new, "single Write -> new file");
    }

    #[test]
    fn build_visible_expands_only_active_group() {
        let events = vec![
            ev(3, "/wt/a.rs", 1),
            ev(2, "/wt/a.rs", 2),
            ev(1, "/wt/b.rs", 3),
        ];
        let counts = vec![(1, 0), (1, 0), (1, 0)];
        let groups = build_groups(&events, &counts);

        // active = group 1 (b.rs): headers for both files, b's single edit nested.
        let vis = build_visible(&groups, 1);
        assert_eq!(
            vis,
            vec![
                VisibleRow::Header { group: 0 },
                VisibleRow::Header { group: 1 },
                VisibleRow::Edit { event: 2 },
            ]
        );

        // active = group 0 (a.rs): a's two edits nested, b folded.
        let vis = build_visible(&groups, 0);
        assert_eq!(
            vis,
            vec![
                VisibleRow::Header { group: 0 },
                VisibleRow::Edit { event: 0 },
                VisibleRow::Edit { event: 1 },
                VisibleRow::Header { group: 1 },
            ]
        );
    }

    #[test]
    fn reclamp_split_shrinks_to_fit_small_area() {
        let mut app = App::bare(PathBuf::from("/wt"));
        app.list_width = 32; // the default
        app.last_area = Rect::new(0, 0, 40, 10); // max = 40 - MIN_DIFF(24) - 1 = 15 → floored to MIN_LIST
        app.reclamp_split();
        assert_eq!(app.list_width, MIN_LIST);
        // A comfortably wide area leaves the width untouched.
        app.list_width = 32;
        app.last_area = Rect::new(0, 0, 120, 10);
        app.reclamp_split();
        assert_eq!(app.list_width, 32);
    }
}
