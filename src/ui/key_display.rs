//! The screenkey/KeyCastr-style overlay chip (`--show-keys`/`[ui]
//! show_keys`): shows the most recently pressed key(s), in the keymap's own
//! compact notation, for recordings and pair-review demos where a viewer
//! can't otherwise see what's being pressed.
//!
//! [`KeyDisplayState::record_step`] is fed the exact same
//! `(chord, StepResult)` pair `ui::mod`'s event loop just got back from
//! [`crate::keymap::Resolver::feed`], so a multi-key sequence's
//! in-progress prefix (`g` while waiting for a second `g`/`d`) reflects the
//! trie's real resolution state rather than a second, independently-tracked
//! copy of it that could drift out of sync.
//!
//! Deliberately never echoes typed *characters*: while a text-input overlay
//! (comment compose, the scope menu's revision field) has focus, the event
//! loop calls [`KeyDisplayState::record_typing`] instead of
//! [`Self::record_step`], which shows a generic `[typing…]` placeholder no
//! matter what was actually pressed. A reviewer's in-progress comment text
//! is private review commentary, not a keystroke to demonstrate — echoing
//! it into an overlay built for screen recordings would leak content that
//! was never meant to be on screen, script or no script.

use crate::keymap::{KeyChord, KeySeq, StepResult};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use std::time::{Duration, Instant};

/// How long a resolved key/sequence stays in the chip before clearing.
/// Terminal cells have no alpha channel, so "fade" here just means
/// "disappear after this long" — checked once per event-loop tick via
/// [`KeyDisplayState::tick`], the same way `ui::mod`'s `WatchStatus` ages
/// out a watch-mode note after `WATCH_STATUS_FLASH`.
const FADE_TIMEOUT: Duration = Duration::from_millis(1500);

/// What the overlay chip shows right now, and the bookkeeping behind it:
/// the in-progress prefix of a multi-key sequence, the most recently
/// resolved key/sequence, a `×N` collapse of repeated identical presses
/// (`j ×3`), or the `[typing…]` placeholder — see the module docs.
///
/// `enabled` gates every method here, not just [`render`]: a session that
/// never asked for `--show-keys`/`[ui] show_keys` (the common case, since
/// it defaults off) pays nothing beyond one `bool` check per keystroke.
pub struct KeyDisplayState {
    enabled: bool,
    /// Chords fed since the last resolved (or cancelled) sequence —
    /// mirrors what `Resolver`'s own `pending` field holds, kept here too
    /// since the resolver clears its copy the instant a sequence resolves,
    /// before this type gets a chance to render the completed notation.
    pending: Vec<KeyChord>,
    text: String,
    /// The notation of the last *completed* press, for detecting a repeat
    /// (`j` then `j` again → `j ×2`). `None` mid-sequence and right after a
    /// fade, so an unrelated key never accidentally continues a stale
    /// streak.
    last_notation: Option<String>,
    repeat_count: u32,
    last_update: Option<Instant>,
}

impl KeyDisplayState {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            pending: Vec::new(),
            text: String::new(),
            last_notation: None,
            repeat_count: 0,
            last_update: None,
        }
    }

    /// Feeds one `Resolver::feed` outcome into the display. Call this right
    /// alongside the `resolver.feed(chord)` call itself, with the exact
    /// same `chord` and the `StepResult` it returned.
    pub fn record_step(&mut self, chord: KeyChord, result: StepResult, now: Instant) {
        if !self.enabled {
            return;
        }
        self.pending.push(chord);
        match result {
            StepResult::Pending => {
                self.text = KeySeq::from_chords(self.pending.clone()).compact_notation();
                self.last_update = Some(now);
                // Mid-sequence, not a completed press — breaks any `×N`
                // streak in progress rather than let an unrelated prefix
                // silently continue it.
                self.last_notation = None;
                self.repeat_count = 0;
            }
            StepResult::Matched(_) | StepResult::Cancelled => {
                let notation =
                    KeySeq::from_chords(std::mem::take(&mut self.pending)).compact_notation();
                self.repeat_count = if self.last_notation.as_deref() == Some(notation.as_str()) {
                    self.repeat_count + 1
                } else {
                    1
                };
                self.text = if self.repeat_count > 1 {
                    format!("{notation} \u{d7}{}", self.repeat_count)
                } else {
                    notation.clone()
                };
                self.last_notation = Some(notation);
                self.last_update = Some(now);
            }
        }
    }

    /// Text-input contexts (comment compose, the scope menu's revision
    /// field) call this once per keystroke instead of [`Self::record_step`]
    /// — see the module docs on why raw characters are never shown.
    pub fn record_typing(&mut self, now: Instant) {
        if !self.enabled {
            return;
        }
        self.pending.clear();
        self.text = "[typing\u{2026}]".to_owned();
        self.last_notation = None;
        self.repeat_count = 0;
        self.last_update = Some(now);
    }

    /// Clears the chip once [`FADE_TIMEOUT`] has passed since the last key
    /// — call once per event-loop iteration, regardless of whether this
    /// iteration saw a key at all, the same way `ui::mod`'s `WatchStatus`
    /// ages out. A no-op once already clear.
    pub fn tick(&mut self, now: Instant) {
        if let Some(last) = self.last_update
            && now.duration_since(last) > FADE_TIMEOUT
        {
            self.text.clear();
            self.last_notation = None;
            self.repeat_count = 0;
            self.last_update = None;
        }
    }

    /// The chip's current text, or `None` when there's nothing to show
    /// (disabled, or faded out) — [`render`] draws nothing in that case.
    fn text(&self) -> Option<&str> {
        if self.text.is_empty() {
            None
        } else {
            Some(&self.text)
        }
    }
}

/// Draws the chip over `area`'s bottom-right corner, one row tall — the
/// same "float over the content area's corner, don't reshape it" approach
/// `hover_popup`/`compose` use for their own overlays, just anchored to a
/// corner instead of the cursor. A no-op when there's nothing to show, so a
/// disabled (the default) or faded-out chip costs one `render_widget` less,
/// not a widget drawn empty.
pub fn render(frame: &mut Frame, area: Rect, state: &KeyDisplayState) {
    let Some(text) = state.text() else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let label = format!(" {text} ");
    let width = (label.chars().count() as u16).min(area.width);
    let rect = Rect {
        x: area.x + area.width - width,
        y: area.y + area.height - 1,
        width,
        height: 1,
    };
    frame.render_widget(Clear, rect);
    let style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    frame.render_widget(Paragraph::new(Line::from(Span::styled(label, style))), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers};

    fn chord(c: char) -> KeyChord {
        KeyChord::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn base_time() -> Instant {
        Instant::now()
    }

    #[test]
    fn disabled_state_never_shows_anything() {
        let mut state = KeyDisplayState::new(false);
        let now = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), None);
    }

    #[test]
    fn a_single_key_binding_shows_its_own_notation() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), Some("j"));
    }

    #[test]
    fn a_multi_key_sequence_accumulates_then_shows_the_compact_notation() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(chord('g'), StepResult::Pending, now);
        assert_eq!(state.text(), Some("g"));
        state.record_step(
            chord('d'),
            StepResult::Matched(crate::keymap::Action::GotoDefinition),
            now,
        );
        assert_eq!(state.text(), Some("gd"));
    }

    #[test]
    fn consecutive_identical_presses_collapse_with_a_count() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), Some("j"));
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), Some("j \u{d7}2"));
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), Some("j \u{d7}3"));
    }

    #[test]
    fn a_different_key_breaks_the_collapse_streak() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), Some("j \u{d7}2"));
        state.record_step(
            chord('k'),
            StepResult::Matched(crate::keymap::Action::CursorUp),
            now,
        );
        assert_eq!(state.text(), Some("k"));
    }

    #[test]
    fn a_pending_prefix_breaks_a_running_collapse_streak() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(state.text(), Some("j \u{d7}2"));
        // `g` starts an unrelated multi-key sequence — even if it happened
        // to resolve back to a single `j`-shaped notation later, the
        // streak already broke the moment a different prefix started.
        state.record_step(chord('g'), StepResult::Pending, now);
        state.record_step(
            chord('g'),
            StepResult::Matched(crate::keymap::Action::Top),
            now,
        );
        assert_eq!(state.text(), Some("gg"));
    }

    #[test]
    fn cancelled_sequences_still_display_what_was_typed() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(chord('g'), StepResult::Pending, now);
        state.record_step(chord('x'), StepResult::Cancelled, now);
        assert_eq!(state.text(), Some("gx"));
    }

    #[test]
    fn record_typing_shows_a_generic_placeholder_never_the_typed_character() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_typing(now);
        assert_eq!(state.text(), Some("[typing\u{2026}]"));
    }

    #[test]
    fn typing_placeholder_breaks_a_running_collapse_streak() {
        let mut state = KeyDisplayState::new(true);
        let now = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        state.record_typing(now);
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            now,
        );
        assert_eq!(
            state.text(),
            Some("j"),
            "typing in between must reset the streak back to a fresh single press"
        );
    }

    #[test]
    fn tick_clears_the_chip_after_the_fade_timeout() {
        let mut state = KeyDisplayState::new(true);
        let start = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            start,
        );
        assert_eq!(state.text(), Some("j"));

        state.tick(start + Duration::from_millis(500));
        assert_eq!(state.text(), Some("j"), "not faded yet");

        state.tick(start + FADE_TIMEOUT + Duration::from_millis(1));
        assert_eq!(state.text(), None, "faded after the timeout");
    }

    #[test]
    fn a_fresh_press_after_a_fade_starts_the_collapse_count_over() {
        let mut state = KeyDisplayState::new(true);
        let start = base_time();
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            start,
        );
        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            start,
        );
        assert_eq!(state.text(), Some("j \u{d7}2"));

        let later = start + FADE_TIMEOUT + Duration::from_millis(1);
        state.tick(later);
        assert_eq!(state.text(), None);

        state.record_step(
            chord('j'),
            StepResult::Matched(crate::keymap::Action::CursorDown),
            later,
        );
        assert_eq!(
            state.text(),
            Some("j"),
            "a fresh press after the chip faded starts a new streak, not ×3"
        );
    }

    #[test]
    fn disabled_state_ignores_typing_and_tick_too() {
        let mut state = KeyDisplayState::new(false);
        let now = base_time();
        state.record_typing(now);
        assert_eq!(state.text(), None);
        state.tick(now + FADE_TIMEOUT * 2);
        assert_eq!(state.text(), None);
    }
}
