//! The active screen the event loop drives, and the stack of them. M2 only
//! ever runs with a stack of one — `ktmr diff` starts with [`View::Diff`],
//! `ktmr open` with [`View::File`] — but the stack exists now so a later
//! milestone's "go to definition" can [`ViewStack::push`] a [`View::File`]
//! on top of whatever the user was looking at and pop back to it, without
//! `ui::run`'s event loop changing shape.

use crate::keymap::Action;
use crate::lsp::DiagnosticsStore;
use crate::ui::app::App;
use crate::ui::file_view::FileView;
use crate::ui::hover_popup::HoverQuery;
use crate::ui::log_view::LogView;
use crate::ui::timeline_view::TimelineView;

/// One screen's worth of state. The event loop talks to whichever variant
/// is on top of the [`ViewStack`] only through this enum — never reaching
/// into `App`, `FileView`, or `TimelineView` fields directly — so adding
/// another view later means adding one more match arm here, not touching
/// the loop.
pub enum View {
    Diff(App),
    File(FileView),
    Timeline(TimelineView),
    Log(LogView),
}

impl View {
    pub fn should_quit(&self) -> bool {
        match self {
            View::Diff(app) => app.should_quit,
            View::File(file) => file.should_quit,
            View::Timeline(timeline) => timeline.should_quit,
            View::Log(log) => log.should_quit,
        }
    }

    pub fn update(&mut self, action: Action) {
        match self {
            View::Diff(app) => app.update(action),
            View::File(file) => file.update(action),
            View::Timeline(timeline) => timeline.update(action),
            View::Log(log) => log.update(action),
        }
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        match self {
            View::Diff(app) => app.set_viewport_height(height),
            View::File(file) => file.set_viewport_height(height),
            View::Timeline(timeline) => timeline.set_viewport_height(height),
            View::Log(log) => log.set_viewport_height(height),
        }
    }

    pub fn set_pending_keys(&mut self, keys: String) {
        match self {
            View::Diff(app) => app.pending_keys = keys,
            View::File(file) => file.pending_keys = keys,
            View::Timeline(timeline) => timeline.pending_keys = keys,
            View::Log(log) => log.pending_keys = keys,
        }
    }

    pub fn clear_pending_keys(&mut self) {
        self.set_pending_keys(String::new());
    }

    /// What `Action::Hover` should ask a language server about, per
    /// whichever view is on top — see [`App::hover_query`] /
    /// [`FileView::hover_query`].
    pub fn hover_query(&self) -> Option<HoverQuery> {
        match self {
            View::Diff(app) => app.hover_query(),
            View::File(file) => file.hover_query(),
            View::Timeline(timeline) => timeline.hover_query(),
            View::Log(log) => log.hover_query(),
        }
    }

    /// `(cursor, active_symbol)` — a cheap, comparable snapshot of "what's
    /// under the cursor for hover purposes." `ui::mod`'s event loop
    /// compares this before and after every action to decide whether an
    /// open or in-flight hover popup is now stale, without needing to know
    /// which actions move the cursor or the active symbol and which don't.
    pub fn hover_cursor_key(&self) -> (usize, usize) {
        match self {
            View::Diff(app) => (app.cursor, app.active_symbol),
            View::File(file) => (file.cursor, file.active_symbol),
            View::Timeline(timeline) => timeline.cursor_key(),
            View::Log(log) => log.cursor_key(),
        }
    }

    /// The cursor's row within the view's content pane, for positioning the
    /// hover popup near it. `None` when the cursor is above the current
    /// scroll offset (shouldn't happen in practice — the cursor is always
    /// kept on screen — but a popup with nowhere sensible to anchor is
    /// simply not drawn rather than drawn somewhere wrong).
    pub fn cursor_screen_row(&self) -> Option<u16> {
        let (cursor, scroll_offset) = match self {
            View::Diff(app) => (app.cursor, app.scroll_offset),
            View::File(file) => (file.cursor, file.scroll_offset),
            // Never opens a hover popup — see `TimelineView::hover_query`'s
            // / `LogView::hover_query`'s docs — so there's no anchor row to
            // report here.
            View::Timeline(_) | View::Log(_) => return None,
        };
        u16::try_from(cursor.checked_sub(scroll_offset)?).ok()
    }

    /// Moves the cursor to the next/previous row bearing a diagnostic —
    /// `Action::NextDiagnostic`/`PrevDiagnostic`, intercepted by
    /// `ui::mod`'s event loop the same way `Action::Hover` is (needs data
    /// `App`/`FileView::update` don't own).
    pub fn jump_to_diagnostic(&mut self, diagnostics: &DiagnosticsStore, forward: bool) {
        match self {
            View::Diff(app) => app.jump_to_diagnostic(diagnostics, forward),
            View::File(file) => file.jump_to_diagnostic(diagnostics, forward),
            View::Timeline(_) | View::Log(_) => {} // no diagnostics in a read-only list
        }
    }
}

/// A non-empty stack of [`View`]s; the top one is what's on screen. Popping
/// below the root view is a no-op rather than an error — the root is what
/// keeps the stack (and therefore the session) alive.
pub struct ViewStack {
    views: Vec<View>,
}

impl ViewStack {
    pub fn new(root: View) -> Self {
        Self { views: vec![root] }
    }

    pub fn push(&mut self, view: View) {
        self.views.push(view);
    }

    /// Pops the top view and reports whether it did. Refuses to pop the last
    /// remaining view — callers use a `false` result as their signal that
    /// the whole session should end instead.
    pub fn pop(&mut self) -> bool {
        if self.views.len() > 1 {
            self.views.pop();
            true
        } else {
            false
        }
    }

    /// Pops every view above the root, restoring the stack to exactly the
    /// one it started the session with. Go-to-definition/references uses
    /// this when a jump lands inside the diff being reviewed: any
    /// `FileView`s pushed by earlier jumps are no longer relevant once the
    /// user has navigated back to content the root diff itself shows.
    pub fn pop_to_root(&mut self) {
        self.views.truncate(1);
    }

    /// Whether the top view is also the root — nothing has been pushed on
    /// top of it. The scope-menu popup's watch-mode pause/resume (see
    /// [`crate::ui::apply_scope_swap`]) only applies when this is true:
    /// watch mode only ever refreshes [`Self::root_mut`], so a scope swap
    /// on a *pushed* diff (opened via `L`/`t`/goto-definition) has nothing
    /// to do with whatever watch mode is refreshing underneath it.
    pub fn is_at_root(&self) -> bool {
        self.views.len() == 1
    }

    pub fn top(&self) -> &View {
        self.views.last().expect("view stack is never empty")
    }

    pub fn top_mut(&mut self) -> &mut View {
        self.views.last_mut().expect("view stack is never empty")
    }

    /// The bottom of the stack — the diff (or single file) the session
    /// started with. Go-to-definition/references checks *this* view, not
    /// whichever one is currently on top, when deciding whether a jump
    /// target already appears in the diff under review — see
    /// [`crate::ui::navigation::navigate_to`].
    pub fn root_mut(&mut self) -> &mut View {
        self.views.first_mut().expect("view stack is never empty")
    }
}
