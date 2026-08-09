//! Turns the *active* keymap into the compact, curated hint lists every
//! status bar shows, and wraps them to fit the terminal instead of
//! truncating. Both fix the same underlying report: a user who didn't know
//! `Ctrl-o`/`Ctrl-t` (jump back/forward) existed, because the hint that
//! names them sat mid-line in a fixed single-row status bar and got cut off
//! on anything but a wide terminal — and, separately, that the hint text was
//! hardcoded vim notation, so it kept showing `gd`/`gr` even under
//! `keymap = "emacs"`.
//!
//! [`HintItem::for_actions`] is the fix for the second problem: it reads
//! each hinted action's binding out of the live [`Keymap`] (preset plus any
//! `[keys]` override) rather than a string literal, so a rebind or a preset
//! switch changes hint text with no separate update needed anywhere.
//! [`wrap_items`]/[`required_height`] are the fix for the first: hint items
//! wrap onto additional lines, up to [`MAX_HINT_LINES`], instead of running
//! off the edge — see `required_height`'s docs for how a layout function
//! uses it to size the status bar before the frame is split.

use crate::keymap::{Action, Keymap};
use crate::ui::text::display_width;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// The most rows a status bar's hint area will ever grow to, however many
/// curated items a view lists or how narrow the terminal is. Chosen so the
/// worst case (a narrow terminal under the emacs preset, whose multi-chord
/// notation like `M-g M-n` runs wider than vim's) still leaves the content
/// pane the majority of the screen: 3 hint rows plus the 1-row info line
/// caps the whole status bar at 4 rows, which only matters at all on a
/// terminal too short to spare much more than that anyway. Items beyond
/// this cap are dropped from the end — see [`wrap_items`] — so a curated
/// list's most important entries (ordered first by each view's item
/// builder) are always what survives.
pub const MAX_HINT_LINES: usize = 3;

/// One hint entry: the key(s) that trigger it and a short label, displayed
/// as `"{keys} {label}"` (e.g. `"gd def"`) and wrapped as a single atomic
/// unit — see [`wrap_items`] — so a narrow terminal never splits a hint
/// mid-item the way ordinary word-wrap would.
pub struct HintItem {
    keys: String,
    label: &'static str,
}

impl HintItem {
    /// Builds one hint item by looking up each of `actions` in `keymap` and
    /// joining whichever bindings exist with `/` (e.g. `CursorDown` +
    /// `CursorUp` → `"j/k"`) — the multi-action form covers both a single
    /// hinted action (`&[Action::GotoDefinition]` → `"gd"`) and the
    /// up/down-style pairs the old hardcoded hints combined into one entry,
    /// through the same call rather than two near-duplicate constructors.
    ///
    /// `None` only if *none* of `actions` has a binding — a hint naming zero
    /// working keys would be actively misleading, so it's simply omitted
    /// (callers `.flatten()` a list of these). If at least one does, the
    /// item is built from whichever bindings are present; in practice every
    /// action this is called with always has one (both built-in presets
    /// bind all 33 actions, and `[keys]` only rebinds/appends — see
    /// `Keymap::binding_for`'s docs), so this only ever loses a pair down to
    /// its single working half, never disappears outright.
    pub fn for_actions(keymap: &Keymap, actions: &[Action], label: &'static str) -> Option<Self> {
        let keys: Vec<String> = actions
            .iter()
            .filter_map(|a| keymap.binding_for(*a))
            .map(|seq| seq.compact_notation())
            .collect();
        if keys.is_empty() {
            return None;
        }
        Some(Self {
            keys: keys.join("/"),
            label,
        })
    }

    fn display(&self) -> String {
        format!("{} {}", self.keys, self.label)
    }

    fn width(&self) -> usize {
        display_width(&self.display())
    }

    #[cfg(test)]
    fn for_test(keys: &str, label: &'static str) -> Self {
        Self {
            keys: keys.to_owned(),
            label,
        }
    }
}

/// The separator between hint items on the same line — two spaces, matching
/// the old hardcoded hint strings' own spacing between entries.
const ITEM_SEP: &str = "  ";

/// Greedily packs `items` (most-important-first — see each view's item-list
/// builder below) onto lines of at most `width` display columns, never
/// splitting an item across a line boundary, and stops after `max_lines`
/// lines — anything left over is simply dropped, so a curated list's least
/// important, latest-listed entries are the first to go on a very narrow
/// terminal rather than an arbitrary truncation mid-item.
///
/// A single item wider than `width` on its own still gets its own line
/// (left to overflow) rather than being dropped outright — the same
/// "don't character-split, don't silently vanish" policy
/// [`crate::ui::text::wrap_text`] applies to an overlong word.
pub fn wrap_items(items: &[HintItem], width: usize, max_lines: usize) -> Vec<String> {
    let sep_width = display_width(ITEM_SEP);
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for item in items {
        if lines.len() >= max_lines {
            break;
        }
        let item_width = item.width();
        let fits = current.is_empty() || current_width + sep_width + item_width <= width;

        if !fits {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
            if lines.len() >= max_lines {
                break;
            }
        }

        if !current.is_empty() {
            current.push_str(ITEM_SEP);
            current_width += sep_width;
        }
        current.push_str(&item.display());
        current_width += item_width;
    }

    if !current.is_empty() && lines.len() < max_lines {
        lines.push(current);
    }
    lines
}

/// The bullet + space [`render_lines`] prefixes every wrapped hint row
/// with, kept as a named constant (rather than a literal repeated in both
/// [`render_lines`] and [`wrap_for_area`]) so the two never drift apart —
/// they did, briefly, during development: `wrap_items` wrapped against the
/// *full* area width while `render_lines` then added two more columns of
/// bullet on top, silently clipping the last character or two of a line
/// that had wrapped to exactly fit.
const LINE_PREFIX: &str = "\u{00B7} ";

/// [`wrap_items`] against `area_width` minus [`LINE_PREFIX`]'s own width —
/// the width actually available to hint text once the bullet prefix
/// [`render_lines`] adds is accounted for. The one place that reservation
/// happens; [`required_height`] and every status-bar renderer call this
/// (never `wrap_items` directly with a raw area width) so the row count a
/// layout reserves and the text a renderer wraps into it always agree.
pub fn wrap_for_area(items: &[HintItem], area_width: u16) -> Vec<String> {
    let width = (area_width as usize).saturating_sub(display_width(LINE_PREFIX));
    wrap_items(items, width, MAX_HINT_LINES)
}

/// How many rows the status bar needs this frame: one for the existing info
/// line (repo/position/pending-keys/notes — unchanged by M9) plus however
/// many rows [`wrap_for_area`] uses to fit the view's hints in `width`
/// columns.
///
/// Layout functions (`ui::mod::diff_layout`, `file_view::layout`,
/// `timeline_view::layout`) call this *before* splitting the frame area —
/// unlike the old fixed one-row status bar, the main content pane now
/// shrinks by exactly as many rows as this frame's hints actually need,
/// rather than a constant that either wasted space (short hint list, wide
/// terminal) or truncated (long hint list, narrow terminal).
pub fn required_height(items: &[HintItem], width: u16) -> u16 {
    1 + wrap_for_area(items, width).len() as u16
}

/// Renders already-wrapped hint rows (from [`wrap_for_area`]) as additional
/// status-bar lines, styled to match the old single-line hint text's dim,
/// secondary look (a leading bullet, `Color::DarkGray`). Shared by
/// `status_bar`, `file_view`, and `timeline_view`'s status renderers so all
/// three stay visually identical rather than drifting if only one were
/// touched later.
pub fn render_lines(wrapped: &[String]) -> Vec<Line<'static>> {
    wrapped
        .iter()
        .map(|line| {
            Line::from(Span::styled(
                format!("{LINE_PREFIX}{line}"),
                Style::default().fg(Color::DarkGray),
            ))
        })
        .collect()
}

/// [`View::Diff`](crate::ui::View::Diff)'s curated hints, most-useful-first.
/// Jump back/forward (`Ctrl-o`/`Ctrl-t`) sits second, right after basic
/// cursor movement — this is the M9 discoverability fix: the binding always
/// existed, but the old fixed-width, non-wrapping status bar buried it
/// eleventh out of sixteen items, past where most terminals cut the line
/// off. Everything below is the same set the pre-M9 hardcoded `HINTS`
/// constant listed, just reordered.
pub fn diff_view_items(keymap: &Keymap) -> Vec<HintItem> {
    [
        HintItem::for_actions(keymap, &[Action::CursorDown, Action::CursorUp], "move"),
        HintItem::for_actions(keymap, &[Action::JumpBack, Action::JumpForward], "jump"),
        // Early, not tucked at the end with the other toggles below: the
        // whole point of `?` is discoverability, so it needs to survive
        // `wrap_items`' first-N-items-fit cutoff on a realistic-width
        // terminal (see
        // `fold_and_help_hints_do_not_push_quit_off_the_status_bar_at_a_realistic_width`
        // below) rather than being the kind of low-priority entry that
        // policy is designed to drop first.
        HintItem::for_actions(keymap, &[Action::OpenHelp], "help"),
        HintItem::for_actions(keymap, &[Action::ToggleLspInspector], "LSP log"),
        HintItem::for_actions(keymap, &[Action::GotoDefinition], "def"),
        HintItem::for_actions(keymap, &[Action::FindReferences], "refs"),
        HintItem::for_actions(keymap, &[Action::Top, Action::Bottom], "top/bottom"),
        HintItem::for_actions(
            keymap,
            &[Action::HalfPageDown, Action::HalfPageUp],
            "half-page",
        ),
        HintItem::for_actions(keymap, &[Action::NextHunk, Action::PrevHunk], "hunk"),
        HintItem::for_actions(keymap, &[Action::ExpandFold, Action::CollapseFold], "fold"),
        HintItem::for_actions(keymap, &[Action::NextFile, Action::PrevFile], "file"),
        HintItem::for_actions(keymap, &[Action::Hover], "hover"),
        HintItem::for_actions(
            keymap,
            &[Action::NextDiagnostic, Action::PrevDiagnostic],
            "diag",
        ),
        HintItem::for_actions(keymap, &[Action::OpenScopeMenu], "scope"),
        // Moderate priority: below `help`'s deliberately-early placement,
        // but well ahead of the lower-traffic toggles further down. `n`/`N`
        // get no hint item of their own — they only mean anything once a
        // search is already confirmed, at which point the prompt echo (and
        // the "search wrapped"/"no matches" notes) are the discoverability
        // signal, not the status bar's hint row; `?`'s help window is where
        // they're documented (see `ui::help::describe`).
        HintItem::for_actions(keymap, &[Action::OpenSearch], "search"),
        HintItem::for_actions(keymap, &[Action::AddComment], "comment"),
        HintItem::for_actions(keymap, &[Action::ToggleSidebar], "sidebar"),
        HintItem::for_actions(keymap, &[Action::ToggleLayout], "layout"),
        HintItem::for_actions(keymap, &[Action::NextSymbol], "symbol"),
        HintItem::for_actions(keymap, &[Action::ToggleComments], "toggle"),
        HintItem::for_actions(keymap, &[Action::Quit], "quit"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// [`View::File`](crate::ui::View::File)'s curated hints — the same
/// reordering rationale as [`diff_view_items`] (jump promoted to second),
/// trimmed to the subset that applies outside a diff (no hunk/file
/// navigation, no comments, no sidebar/layout toggles).
pub fn file_view_items(keymap: &Keymap) -> Vec<HintItem> {
    [
        HintItem::for_actions(keymap, &[Action::CursorDown, Action::CursorUp], "move"),
        HintItem::for_actions(keymap, &[Action::JumpBack, Action::JumpForward], "jump"),
        HintItem::for_actions(keymap, &[Action::OpenHelp], "help"),
        HintItem::for_actions(keymap, &[Action::ToggleLspInspector], "LSP log"),
        HintItem::for_actions(keymap, &[Action::GotoDefinition], "def"),
        HintItem::for_actions(keymap, &[Action::FindReferences], "refs"),
        HintItem::for_actions(keymap, &[Action::Top, Action::Bottom], "top/bottom"),
        HintItem::for_actions(
            keymap,
            &[Action::HalfPageDown, Action::HalfPageUp],
            "half-page",
        ),
        HintItem::for_actions(
            keymap,
            &[Action::NextDiagnostic, Action::PrevDiagnostic],
            "diag",
        ),
        HintItem::for_actions(keymap, &[Action::Hover], "hover"),
        HintItem::for_actions(keymap, &[Action::NextSymbol], "symbol"),
        HintItem::for_actions(keymap, &[Action::Quit], "quit"),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// [`View::Timeline`](crate::ui::View::Timeline)'s curated hints. No jump
/// item here — unlike the diff/file views, `Ctrl-o`/`Ctrl-t` have nothing to
/// retrace from a read-only timeline (see `View::jump_entry`'s docs on why
/// it always returns `None` for this view).
pub fn timeline_view_items(keymap: &Keymap) -> Vec<HintItem> {
    [
        HintItem::for_actions(keymap, &[Action::CursorDown, Action::CursorUp], "select"),
        HintItem::for_actions(keymap, &[Action::OpenHelp], "help"),
        HintItem::for_actions(keymap, &[Action::ToggleLspInspector], "LSP log"),
        HintItem::for_actions(keymap, &[Action::NextSymbol], "focus"),
        HintItem::for_actions(keymap, &[Action::ToggleRangeSelect], "range"),
        HintItem::for_actions(keymap, &[Action::Confirm], "back to diff"),
        HintItem::for_actions(
            keymap,
            &[Action::Quit, Action::Cancel, Action::ToggleTimeline],
            "close",
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// [`View::Log`](crate::ui::View::Log)'s curated hints. No jump item, the
/// same reason [`timeline_view_items`] has none — a read-only history list
/// has nowhere for `Ctrl-o`/`Ctrl-i` to retrace from (see
/// `View::jump_entry`'s docs).
pub fn log_view_items(keymap: &Keymap) -> Vec<HintItem> {
    [
        HintItem::for_actions(keymap, &[Action::CursorDown, Action::CursorUp], "select"),
        HintItem::for_actions(keymap, &[Action::OpenHelp], "help"),
        HintItem::for_actions(keymap, &[Action::ToggleLspInspector], "LSP log"),
        HintItem::for_actions(keymap, &[Action::Confirm], "open diff"),
        HintItem::for_actions(keymap, &[Action::ToggleRangeSelect], "range"),
        HintItem::for_actions(
            keymap,
            &[Action::Quit, Action::Cancel, Action::ToggleLogView],
            "close",
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{KeySeq, emacs_preset, vim_preset};

    #[test]
    fn for_actions_reads_the_live_vim_binding() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let item = HintItem::for_actions(&keymap, &[Action::GotoDefinition], "def").unwrap();
        assert_eq!(item.display(), "gd def");
    }

    #[test]
    fn for_actions_reads_the_live_emacs_binding() {
        // The M9 bug fix, pinned down directly: switching presets changes
        // hint text with no separate update, because this reads the active
        // `Keymap` rather than a hardcoded vim string.
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let item = HintItem::for_actions(&keymap, &[Action::GotoDefinition], "def").unwrap();
        assert_eq!(item.display(), "M-. def");
    }

    #[test]
    fn for_actions_joins_a_pair_with_a_slash() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let item = HintItem::for_actions(&keymap, &[Action::CursorDown, Action::CursorUp], "move")
            .unwrap();
        assert_eq!(item.display(), "j/k move");
    }

    #[test]
    fn for_actions_follows_a_keys_override() {
        let mut bindings = vim_preset(false);
        bindings
            .iter_mut()
            .find(|(_, a)| *a == Action::GotoDefinition)
            .unwrap()
            .0 = KeySeq::parse("End");
        let keymap = Keymap::from_bindings(&bindings);
        let item = HintItem::for_actions(&keymap, &[Action::GotoDefinition], "def").unwrap();
        assert_eq!(item.display(), "End def");
    }

    #[test]
    fn for_actions_returns_none_when_nothing_in_the_list_is_bound() {
        let keymap = Keymap::from_bindings(&[(KeySeq::parse("j"), Action::CursorDown)]);
        assert!(HintItem::for_actions(&keymap, &[Action::Quit], "quit").is_none());
    }

    #[test]
    fn wrap_items_keeps_everything_on_one_line_when_it_fits() {
        let items = vec![
            HintItem::for_test("j/k", "move"),
            HintItem::for_test("gd", "def"),
        ];
        assert_eq!(
            wrap_items(&items, 80, MAX_HINT_LINES),
            vec!["j/k move  gd def"]
        );
    }

    #[test]
    fn wrap_items_starts_a_new_line_when_the_next_item_would_overflow() {
        let items = vec![
            HintItem::for_test("j/k", "move"),
            HintItem::for_test("gd", "def"),
            HintItem::for_test("gr", "refs"),
        ];
        // "j/k move" (8) + sep (2) + "gd def" (6) = 16, exactly fits;
        // "gr refs" (7) more would need 25, too wide for a 16-column line.
        assert_eq!(
            wrap_items(&items, 16, MAX_HINT_LINES),
            vec!["j/k move  gd def", "gr refs"]
        );
    }

    #[test]
    fn wrap_items_never_splits_a_single_item_across_lines() {
        let items = vec![HintItem::for_test("C-o/C-t", "jump")];
        let wrapped = wrap_items(&items, 5, MAX_HINT_LINES);
        assert_eq!(wrapped, vec!["C-o/C-t jump"]); // overflows its budget, but intact
    }

    #[test]
    fn wrap_items_drops_trailing_items_once_max_lines_is_reached() {
        let items = vec![
            HintItem::for_test("a", "one"),
            HintItem::for_test("b", "two"),
            HintItem::for_test("c", "three"),
        ];
        // Force one item per line (narrow width) with a cap of 2 lines —
        // the third, least-important item must be dropped entirely rather
        // than forcing a third line.
        let wrapped = wrap_items(&items, 6, 2);
        assert_eq!(wrapped, vec!["a one", "b two"]);
    }

    #[test]
    fn wrap_items_is_width_aware_for_wide_cjk_labels() {
        // A label containing East Asian wide characters must count as its
        // real terminal-column width (via `display_width`), not its
        // character count — otherwise this would wrongly decide two items
        // fit on one line when they don't.
        let items = vec![
            HintItem::for_test("j", "日本語"), // 1 + 1(space) + 6 = 8 columns
            HintItem::for_test("k", "up"),
        ];
        // "j 日本語" is 8 columns; adding "  k up" (2 + 4 = 6) needs 14,
        // which doesn't fit an 10-column budget.
        assert_eq!(
            wrap_items(&items, 10, MAX_HINT_LINES),
            vec!["j \u{65e5}\u{672c}\u{8a9e}", "k up"]
        );
    }

    #[test]
    fn wrap_for_area_reserves_room_for_the_bullet_prefix_render_lines_adds() {
        // Caught a real bug during development: `wrap_items` wrapped
        // against the *full* area width, but `render_lines` then prefixed
        // every wrapped row with a 2-column bullet on top of that, clipping
        // the last column or two off a line that had wrapped to exactly
        // fill its budget. `wrap_for_area` must reserve that width up
        // front instead.
        let items = vec![
            HintItem::for_test("j/k", "move"),
            HintItem::for_test("gd", "def"),
        ];
        // "j/k move  gd def" is exactly 16 columns. An 18-column area (16
        // content + 2 reserved for the prefix) is the narrowest that still
        // fits both on one line; one column less must wrap, even though 17
        // alone would fit the items' own 16-column width with room to
        // spare — the 2 reserved columns are what force it.
        assert_eq!(wrap_for_area(&items, 18), vec!["j/k move  gd def"]);
        assert_eq!(wrap_for_area(&items, 17), vec!["j/k move", "gd def"]);
    }

    #[test]
    fn required_height_is_one_when_hints_fit_on_a_single_line() {
        let items = vec![HintItem::for_test("j/k", "move")];
        assert_eq!(required_height(&items, 80), 1 + 1);
    }

    #[test]
    fn required_height_grows_with_wrapped_lines_and_caps_at_max_hint_lines() {
        let items = vec![
            HintItem::for_test("a", "one"),
            HintItem::for_test("b", "two"),
            HintItem::for_test("c", "three"),
            HintItem::for_test("d", "four"),
            HintItem::for_test("e", "five"),
        ];
        // Each item needs its own line at this width; capped at
        // `MAX_HINT_LINES` (3) hint rows plus the 1 info row, even though 5
        // items would otherwise need 5 lines.
        assert_eq!(required_height(&items, 4), 1 + MAX_HINT_LINES as u16);
    }

    #[test]
    fn required_height_is_one_when_there_are_no_items() {
        assert_eq!(required_height(&[], 80), 1);
    }

    #[test]
    fn diff_view_items_lists_jump_back_forward_second() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let items = diff_view_items(&keymap);
        assert_eq!(items[0].display(), "j/k move");
        assert_eq!(items[1].display(), "C-o/C-t jump");
    }

    #[test]
    fn file_view_items_lists_jump_back_forward_second() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let items = file_view_items(&keymap);
        assert_eq!(items[0].display(), "j/k move");
        assert_eq!(items[1].display(), "C-o/C-t jump");
    }

    #[test]
    fn diff_view_items_show_emacs_notation_under_the_emacs_preset() {
        let keymap = Keymap::from_bindings(&emacs_preset(false));
        let items = diff_view_items(&keymap);
        let def = items.iter().find(|i| i.label == "def").unwrap();
        assert_eq!(def.display(), "M-. def");
        let refs = items.iter().find(|i| i.label == "refs").unwrap();
        assert_eq!(refs.display(), "M-? refs");
    }

    /// `diff_view_items` was already close to filling [`MAX_HINT_LINES`] at
    /// a realistic 100-column pane before the fold hint existed — this
    /// pins down that adding it (then the `?` help hint, then Issue #5's
    /// `/ search` hint) didn't quietly push a lower-priority item (here,
    /// `quit`, the last-listed and therefore first-dropped one per
    /// [`wrap_items`]'s policy) off the status bar entirely. `help` itself
    /// is listed early (see `diff_view_items`'s own item list)
    /// specifically so it isn't the one that goes missing here instead —
    /// discoverability is the entire point of `?`.
    #[test]
    fn fold_and_help_hints_do_not_push_quit_off_the_status_bar_at_a_realistic_width() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let items = diff_view_items(&keymap);
        assert!(
            items.iter().any(|i| i.display().starts_with("zo/zc")),
            "the fold hint should be present in the curated list"
        );
        assert!(
            items.iter().any(|i| i.display() == "? help"),
            "the help hint should be present in the curated list"
        );
        assert!(
            items.iter().any(|i| i.display() == "/ search"),
            "the search hint should be present in the curated list"
        );
        let wrapped = wrap_for_area(&items, 100);
        assert!(
            wrapped.len() <= MAX_HINT_LINES,
            "must still fit within the hint area's line cap"
        );
        assert!(
            wrapped.iter().any(|line| line.contains("? help")),
            "help must be visible at a realistic width; wrapped:\n{wrapped:?}"
        );
        assert!(
            wrapped.iter().any(|line| line.contains("/ search")),
            "search must be visible at a realistic width; wrapped:\n{wrapped:?}"
        );
        assert!(
            wrapped.iter().any(|line| line.contains("quit")),
            "quit must still survive at a realistic width; wrapped:\n{wrapped:?}"
        );
    }

    /// [`file_view_items`]/[`timeline_view_items`]/[`log_view_items`] each
    /// have far more slack than the diff view's own 18-item list (10/5/4
    /// items even after adding `help`), but the same regression is cheap to
    /// guard against for all three: `help` visible, and the list's own
    /// last-listed item (the one [`wrap_items`] would drop first) still
    /// surviving too, at the same realistic 100-column width the diff-view
    /// test above uses.
    #[test]
    fn help_hint_is_visible_in_file_timeline_and_log_lists_at_a_realistic_width() {
        let keymap = Keymap::from_bindings(&vim_preset(false));

        let file_items = file_view_items(&keymap);
        let file_wrapped = wrap_for_area(&file_items, 100);
        assert!(file_wrapped.len() <= MAX_HINT_LINES);
        assert!(file_wrapped.iter().any(|l| l.contains("? help")));
        assert!(file_wrapped.iter().any(|l| l.contains("quit")));

        let timeline_items = timeline_view_items(&keymap);
        let timeline_wrapped = wrap_for_area(&timeline_items, 100);
        assert!(timeline_wrapped.len() <= MAX_HINT_LINES);
        assert!(timeline_wrapped.iter().any(|l| l.contains("? help")));
        assert!(timeline_wrapped.iter().any(|l| l.contains("close")));

        let log_items = log_view_items(&keymap);
        let log_wrapped = wrap_for_area(&log_items, 100);
        assert!(log_wrapped.len() <= MAX_HINT_LINES);
        assert!(log_wrapped.iter().any(|l| l.contains("? help")));
        assert!(log_wrapped.iter().any(|l| l.contains("close")));
    }

    #[test]
    fn timeline_view_items_has_no_jump_item() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let items = timeline_view_items(&keymap);
        assert!(items.iter().all(|i| i.label != "jump"));
    }

    #[test]
    fn render_lines_prefixes_each_wrapped_row_with_a_bullet() {
        let wrapped = vec!["j/k move".to_owned(), "gd def".to_owned()];
        let lines = render_lines(&wrapped);
        assert_eq!(lines.len(), 2);
    }

    /// The M9b payoff: the jump hint's forward key follows whatever
    /// `binding_for(JumpForward)` resolves to (see that method's
    /// first-match-wins docs), with zero hint-side special-casing — a
    /// kitty-protocol-capable session's hint reads `C-o/C-i`, matching
    /// neovim, purely because `vim_preset(true)` lists `C-i` ahead of
    /// `C-t` for that action.
    #[test]
    fn diff_view_items_jump_hint_shows_c_i_when_ci_distinguishable() {
        let keymap = Keymap::from_bindings(&vim_preset(true));
        let items = diff_view_items(&keymap);
        assert_eq!(items[1].display(), "C-o/C-i jump");
    }

    #[test]
    fn diff_view_items_jump_hint_shows_c_t_when_not_ci_distinguishable() {
        let keymap = Keymap::from_bindings(&vim_preset(false));
        let items = diff_view_items(&keymap);
        assert_eq!(items[1].display(), "C-o/C-t jump");
    }

    #[test]
    fn file_view_items_jump_hint_follows_ci_distinguishable_too() {
        // Not diff-view-specific: `file_view_items` builds its jump hint the
        // same way, off the same live keymap.
        let keymap = Keymap::from_bindings(&emacs_preset(true));
        let items = file_view_items(&keymap);
        assert_eq!(items[1].display(), "C-o/C-i jump");
    }
}
