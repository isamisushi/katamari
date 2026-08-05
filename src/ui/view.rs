//! The active screen the event loop drives, and the stack of them. M2 only
//! ever runs with a stack of one — `ktmr diff` starts with [`View::Diff`],
//! `ktmr open` with [`View::File`] — but the stack exists now so a later
//! milestone's "go to definition" can [`ViewStack::push`] a [`View::File`]
//! on top of whatever the user was looking at and pop back to it, without
//! `ui::run`'s event loop changing shape.

use crate::keymap::Action;
use crate::ui::app::App;
use crate::ui::file_view::FileView;

/// One screen's worth of state. The event loop talks to whichever variant
/// is on top of the [`ViewStack`] only through this enum — never reaching
/// into `App` or `FileView` fields directly — so adding a third view later
/// means adding one more match arm here, not touching the loop.
pub enum View {
    Diff(App),
    File(FileView),
}

impl View {
    pub fn should_quit(&self) -> bool {
        match self {
            View::Diff(app) => app.should_quit,
            View::File(file) => file.should_quit,
        }
    }

    pub fn update(&mut self, action: Action) {
        match self {
            View::Diff(app) => app.update(action),
            View::File(file) => file.update(action),
        }
    }

    pub fn set_viewport_height(&mut self, height: usize) {
        match self {
            View::Diff(app) => app.set_viewport_height(height),
            View::File(file) => file.set_viewport_height(height),
        }
    }

    pub fn set_pending_keys(&mut self, keys: String) {
        match self {
            View::Diff(app) => app.pending_keys = keys,
            View::File(file) => file.pending_keys = keys,
        }
    }

    pub fn clear_pending_keys(&mut self) {
        self.set_pending_keys(String::new());
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

    #[allow(dead_code)] // pushed onto from M3's go-to-definition, not yet from M2.
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

    pub fn top(&self) -> &View {
        self.views.last().expect("view stack is never empty")
    }

    pub fn top_mut(&mut self) -> &mut View {
        self.views.last_mut().expect("view stack is never empty")
    }
}
