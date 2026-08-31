//! The M10 milestone's headline pair: does the kitty keyboard protocol
//! probe/reply exchange (M9b) actually work end-to-end against real
//! crossterm parsing, in both directions the real world presents —
//! `kitty_supported` (probe answered with both flags-query and DA1 halves)
//! and `kitty_unsupported` (DA1 only, the tmux/plain-terminal case). Issue
//! #12 extended both with the same real-crossterm-parsing proof for
//! `M-Left`/`M-Right` (the unconditional back/forward aliases) and for
//! `Ctrl-t`/`Ctrl-]` having no default binding at all anymore.
//!
//! Neither test can exercise a genuine jump-stack round trip: every actual
//! push onto `JumpStack` happens either via `gd`/`gr` (needs a live
//! language server, out of scope for this suite's LSP-free fixtures — see
//! `support::fixture`'s docs) or via `Ctrl-o`/`Ctrl-i`/`M-Left`/`M-Right`
//! themselves, which only *pop*, never push
//! (`ui::navigation::navigate_to`'s `record_history` path). So instead of a
//! hollow "press a key, assert nothing crashed" check, both tests press
//! `Ctrl-o` then the mode's jump-forward key against an empty stack and
//! assert the specific "no earlier/later position" status text — proof the
//! actual key bytes on the wire were parsed as `JumpBack`/`JumpForward` and
//! dispatched, not just that the process didn't crash.

use crate::support::harness::SPLASH_MARKER;
use crate::support::screen::underlined_cells;
use crate::support::{Harness, Key, KittyMode, SpawnOptions, fixture};
use std::time::{Duration, Instant};

/// Sends `key` and waits until the status bar shows no `"jump:"` note at
/// all — clearing whatever `Ctrl-o`/`Ctrl-i`/`M-Left`/`M-Right` status text
/// a previous step in the same test left behind (any *matched* action resets
/// the status line before its own handling runs — see
/// `ui::mod::event_loop`'s `StepResult::Matched` arm). Callers use this
/// right before pressing a key whose only observable effect is jump-status
/// text, so a subsequent `wait_for_text` proves that key produced the text,
/// rather than finding text some earlier step already left on screen.
fn clear_jump_status(h: &Harness, key: Key) {
    h.send(key);
    h.wait_until(Duration::from_secs(2), |s| !s.contents().contains("jump:"));
}

/// Asserts `key` never resolves to `JumpForward`/`JumpBack` (or anything
/// else that would touch the status bar's `"jump:"` note). Unlike a
/// positive `wait_for_text` check, there's no later event to wait on that
/// would prove the absence non-racily — every *matched* action resets the
/// status line before its own handling runs (see
/// `ui::mod::event_loop`'s `StepResult::Matched` arm), so pressing a second
/// key afterward to "force a render" would clear a wrongly-set status
/// either way and prove nothing. Follows the same sleep-then-check idiom
/// `tests/e2e/show_keys.rs::without_the_flag_no_key_chip_ever_appears` uses
/// for the identical class of claim ("this must never render"): give the
/// (wrongly bound) action every chance to show up, then assert it didn't.
fn assert_key_has_no_jump_binding(h: &Harness, key: Key) {
    h.send(key);
    std::thread::sleep(Duration::from_millis(200));
    let contents = h.screen_contents();
    assert!(
        !contents.contains("jump:"),
        "expected no default binding, but the status bar shows a jump note; screen:\n{contents}"
    );
}

#[test]
fn kitty_supported() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode: KittyMode::Supported,
            ..Default::default()
        },
    );

    // M9b's discoverability fix, observed for real: a kitty-capable
    // terminal's hint bar names `C-i` as the jump-forward alias, matching
    // neovim. The jump hint only appears in the *expanded* hint bar (the
    // collapsed default shows the minimal subset — see
    // `hints::diff_view_items`), so expand it first with `.`.
    h.wait_for_text("README.md");
    h.send(Key::Char('.'));
    h.wait_for_text("C-o/C-i");

    // `Ctrl-]`/`Ctrl-t` have no default binding at all now (#12 replaces
    // the old legacy-terminal `C-t` fallback with `M-Right`, and never
    // bound a tag-stack key) — check this first, on a completely untouched
    // status bar, so there's nothing else that could be mistaken for it.
    assert_key_has_no_jump_binding(&h, Key::CtrlT);

    // `Ctrl-o` (0x0f, unambiguous in either mode) with an empty back-stack.
    h.send(Key::CtrlO);
    h.wait_for_text("jump: no earlier position");

    // The actual payoff: a kitty-disambiguated `Ctrl-i` (`\x1b[105;5u`) must
    // reach `Action::JumpForward`, not `Action::NextSymbol` — proving
    // crossterm parsed the kitty-protocol escape sequence as a real
    // Ctrl-modified key, distinguishable from the plain Tab this exact byte
    // sequence would otherwise collide with.
    h.send(Key::CtrlI);
    h.wait_for_text("jump: no later position");

    // `M-Left`/`M-Right` are unconditional aliases in both ci-distinguishable
    // states (issue #12) — clear the status line first so the text these
    // produce next proves the alias itself fired, not stale text left over
    // by the C-o/C-i checks above (which read identically for an empty
    // stack either way).
    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltLeft);
    h.wait_for_text("jump: no earlier position");

    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltRight);
    h.wait_for_text("jump: no later position");

    // Tab now means `FocusNextPane`, not `NextSymbol` (issue #13) — and as
    // of issue #14 the root diff view is no longer single-pane, so Tab
    // really does move focus to the files pane rather than being a no-op.
    // Proven the same way `tests/e2e/focus.rs` proves it (no PTY test here
    // inspects cell color): `gd` only ever reports the files-focus-gate
    // note while `Files` owns focus. The active symbol's underline must
    // still not move — focusing a different pane is not the same as
    // cycling the symbol within this one. The two `clear_jump_status`
    // calls above already moved the cursor two rows down onto content
    // (row 3), so there's a real active symbol here to prove didn't move.
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");
    assert_eq!(
        h.with_screen(underlined_cells),
        before,
        "focusing the files pane must not move the diff's active symbol"
    );

    // Tab again cycles back to `Diff` (the root view's only other pane).
    // `l` is the vim preset's real `NextSymbol` binding (issue #13) —
    // sending it must move the active symbol, proving focus genuinely
    // landed back on the diff pane rather than merely that Tab was
    // pressed a second time.
    h.send(Key::Tab);
    h.send(Key::Char('l'));
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn kitty_unsupported() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode: KittyMode::Unsupported,
            ..Default::default()
        },
    );

    // The fallback hint: no kitty protocol, so jump-forward's canonical key
    // is `M-Right` (#12 replaces the old `C-t` fallback). Expanded-bar-only,
    // same as the supported case above.
    h.wait_for_text("README.md");
    h.send(Key::Char('.'));
    h.wait_for_text("C-o/M-Right");

    // `Ctrl-t` has no default binding in this mode either — checked first,
    // on an untouched status bar, same reasoning as `kitty_supported`.
    // `Ctrl-i` itself is checked further down, via the raw Tab byte the two
    // genuinely share on a legacy terminal (`Key::CtrlI` has no legacy
    // encoding at all — see its docs — so it can't be sent here directly).
    assert_key_has_no_jump_binding(&h, Key::CtrlT);

    h.send(Key::CtrlO);
    h.wait_for_text("jump: no earlier position");

    // `M-Right` (`\x1b[1;3C`) is the fallback's real, canonical binding —
    // must reach `JumpForward` when the kitty protocol never activated at
    // all, the same as it does when it did (`kitty_supported` above).
    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltRight);
    h.wait_for_text("jump: no later position");

    // `M-Left` too, as `JumpBack`'s alias.
    clear_jump_status(&h, Key::Char('j'));
    h.send(Key::AltLeft);
    h.wait_for_text("jump: no earlier position");

    // The mirror image of `kitty_supported`'s Tab check: in this mode, raw
    // `0x09` is *only* ever a literal Tab (no `Ctrl-i` binding exists to
    // collide with it) — and, as of issue #13, that literal Tab means
    // `FocusNextPane`, which (issue #14) now really does move focus to the
    // files pane rather than being a no-op — proven the same
    // files-focus-gate way `kitty_supported` proves it. The active
    // symbol's underline must not move, and a literal Tab must still
    // never be mistaken for `Ctrl-i`/`JumpForward` (the actual byte
    // collision this test exists to guard against). The two
    // `clear_jump_status` calls above already moved the cursor two rows
    // down onto content (row 3), so there's a real active symbol here to
    // prove didn't move.
    h.wait_for_text("\u{b7} 3/");
    let before = h.with_screen(underlined_cells);
    h.send(Key::Tab);
    h.send(Key::Char('g'));
    h.send(Key::Char('d'));
    h.wait_for_text("definition: focus the diff pane first");
    assert_eq!(
        h.with_screen(underlined_cells),
        before,
        "focusing the files pane must not move the diff's active symbol"
    );
    assert!(
        !h.screen_contents().contains("jump: no later position"),
        "a literal Tab must never be mistaken for Ctrl-i when the kitty protocol isn't active"
    );

    // Tab again cycles back to `Diff`. `l` is the vim preset's real
    // `NextSymbol` binding — sending it must move the active symbol,
    // unaffected by the kitty protocol not being active (this binding has
    // nothing to do with the Tab/Ctrl-i byte collision above), and proving
    // focus genuinely landed back on the diff pane rather than merely that
    // Tab was pressed a second time.
    h.send(Key::Tab);
    h.send(Key::Char('l'));
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- startup splash vs. a slow-to-answer kitty probe ----------------------

/// The bug this pair of `ui::mod` functions exists to fix, reproduced for
/// real: `enable_kitty_keyboard_protocol`'s probe is a synchronous read that
/// crossterm only bounds at 2s, and on a terminal that never answers, `ktmr`
/// used to sit there with nothing drawn since `init_terminal` entered the
/// alternate screen — indistinguishable from a hang. `probe_reply_delay`
/// (see [`SpawnOptions::probe_reply_delay`]) reproduces that stall on
/// demand, without actually waiting out crossterm's full 2s timeout to
/// prove it: the reply still arrives, just held back long enough (well
/// under crossterm's own bound) for a test to observe the screen mid-wait.
///
/// Uses [`Harness::spawn_without_ready_wait`] rather than the ordinary
/// `Harness::spawn`, since `spawn`'s readiness wait deliberately skips past
/// any frame containing [`SPLASH_MARKER`] — exactly the frame this test
/// needs to catch.
#[test]
fn splash_is_visible_while_the_kitty_probe_is_still_pending() {
    let repo = fixture::basic_repo();
    let mut h = Harness::spawn_without_ready_wait(
        repo.path(),
        SpawnOptions {
            probe_reply_delay: Some(Duration::from_millis(500)),
            ..Default::default()
        },
    );

    // Caught well inside the 500ms the probe reply is being held back —
    // `ktmr` is blocked inside `supports_keyboard_enhancement`'s
    // synchronous read for that whole window, so the real diff view can't
    // have painted yet. Confirms this is genuinely the splash standing in
    // for a still-black screen, not the two racing onto the same frame.
    h.wait_for_text(SPLASH_MARKER);
    assert!(
        !h.screen_contents().contains("README.md"),
        "the real diff view must not have rendered yet while the kitty \
         probe reply is still being withheld"
    );

    // Once the (delayed) reply lands, startup finishes and the splash is
    // replaced by the real UI, same as every other test in this suite.
    h.wait_for_text("README.md");
    assert!(
        !h.screen_contents().contains(SPLASH_MARKER),
        "the splash must not linger once the real UI has painted"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

// ---- kitty probe cache: a second launch skips the probe entirely ---------

/// The actual fix, proven end to end: a first `ktmr` launch in a terminal
/// with no cached verdict yet runs the real probe (as
/// `splash_is_visible_while_the_kitty_probe_is_still_pending` above already
/// proves can stall) and, on answering, must persist that verdict to
/// `ui::probe_cache`'s on-disk cache before this process exits — so a
/// *second* launch against the exact same state directory (and hence the
/// same cache file) finds it there. This harness's terminal fingerprint is
/// always the same across every spawn (`TERM=xterm-256color`, no
/// `TERM_PROGRAM`/`TERM_PROGRAM_VERSION` — see
/// `Harness::spawn_without_ready_wait`), so [`SpawnOptions::state_home`] is
/// the only variable that decides whether the second launch sees a fresh
/// cache (the default, every other test in this file) or a warm one (only
/// here).
#[test]
fn a_second_launch_writes_then_reuses_the_kitty_probe_cache() {
    let repo = fixture::basic_repo();
    let state_home = tempfile::tempdir().expect("tempdir for a shared $XDG_STATE_HOME");
    let cache_path = state_home.path().join("katamari").join("kitty-probe.json");

    // First launch: nothing cached yet, so `ktmr` runs the real probe (this
    // harness's default `KittyMode::Supported`, answered immediately — no
    // `probe_reply_delay` here, this half isn't about the stall) and must
    // write this terminal's fingerprint and verdict before it exits.
    {
        let mut h = Harness::spawn(
            repo.path(),
            SpawnOptions {
                state_home: Some(state_home.path().to_path_buf()),
                ..Default::default()
            },
        );
        h.send(Key::Char('q'));
        let status = h.wait_exit(Duration::from_secs(5));
        assert!(
            status.success(),
            "first launch should exit 0, got {status:?}"
        );
    }

    let cache_text = std::fs::read_to_string(&cache_path).unwrap_or_else(|e| {
        panic!(
            "expected {} to exist after the first launch: {e}",
            cache_path.display()
        )
    });
    let cache_json: serde_json::Value =
        serde_json::from_str(&cache_text).expect("cache file must be valid JSON");
    let entries = cache_json
        .as_object()
        .expect("cache file's top level must be a fingerprint -> verdict object");
    assert_eq!(
        entries.len(),
        1,
        "exactly one fingerprint should have been recorded: {cache_text}"
    );
    let (fingerprint, verdict) = entries.iter().next().unwrap();
    assert!(
        fingerprint.contains("term=xterm-256color"),
        "the recorded fingerprint must be built from this harness's own \
         TERM, got {fingerprint:?}"
    );
    assert_eq!(
        verdict["supported"],
        serde_json::Value::Bool(true),
        "KittyMode::Supported (this harness's default) must cache a `true` \
         verdict: {cache_text}"
    );

    // Second launch, same state home — so the same fingerprint now has a
    // cached verdict — with the probe reply withheld far longer than any
    // real startup should ever take. If the cache is actually consulted
    // (not merely a fast reply racing a slow one), `ktmr` never even sends
    // the probe query, so this reply is never read at all: see
    // `spawn_reader_thread`'s docs on why a withheld reply to a query that
    // was never sent is simply inert.
    let long_delay = Duration::from_secs(4);
    let start = Instant::now();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            state_home: Some(state_home.path().to_path_buf()),
            probe_reply_delay: Some(long_delay),
            ..Default::default()
        },
    );
    let cache_hit_elapsed = start.elapsed();
    assert!(
        h.screen_contents().contains("README.md"),
        "the real diff view, not just any non-splash frame, must have \
         rendered by the time Harness::spawn's readiness wait returns"
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(
        status.success(),
        "second launch should exit 0, got {status:?}"
    );

    // Proof this is a genuine skip, not a hardcoded wall-clock budget on a
    // real fork/exec plus full startup: a third launch against a *fresh*
    // cache (a fresh `state_home`, deliberately not reused from above), with
    // the exact same withheld-reply delay, measures how long a real
    // cache-*miss* launch takes under this run's own scheduling/contention —
    // the number `cache_hit_elapsed` must clear. Comparing the two against
    // each other rather than against a fixed threshold means this assertion
    // tracks relative speedup on whatever machine happens to run it, instead
    // of a millisecond figure tuned to this one.
    let baseline_start = Instant::now();
    let mut baseline = Harness::spawn(
        repo.path(),
        SpawnOptions {
            probe_reply_delay: Some(long_delay),
            ..Default::default()
        },
    );
    let baseline_elapsed = baseline_start.elapsed();
    baseline.send(Key::Char('q'));
    let baseline_status = baseline.wait_exit(Duration::from_secs(5));
    assert!(
        baseline_status.success(),
        "baseline (fresh-cache) launch should exit 0, got {baseline_status:?}"
    );

    assert!(
        cache_hit_elapsed < baseline_elapsed / 2,
        "a cache-hit launch must skip the probe entirely rather than \
         merely outrace a withheld reply; cache-hit ready in \
         {cache_hit_elapsed:?}, a fresh-cache baseline against the same \
         {long_delay:?} withheld reply took {baseline_elapsed:?}"
    );
}

// ---- kitty probe cache: bypassed entirely inside a multiplexer -----------

/// The fix for the fingerprint-collapse bug: inside tmux/screen,
/// `TERM`/`TERM_PROGRAM`/`TERM_PROGRAM_VERSION` no longer identify the outer
/// terminal (tmux overwrites all three to its own identity regardless of
/// which emulator hosts the session — see `ui::probe_cache`'s module docs'
/// **Multiplexers** section), so two different outer terminals — one
/// kitty-capable, one not — would otherwise collapse onto one fingerprint
/// and silently share a verdict that belongs to whichever one happened to
/// probe first. Proven two ways against the same warm cache this test
/// itself seeds: a launch with `$TMUX` set never *records* a verdict at all
/// (so the cache file a plain-terminal launch would have produced here
/// simply doesn't exist), and a launch with `$TMUX` set never *trusts* one
/// either — reusing the exact same-fingerprint warm-cache setup
/// `a_second_launch_writes_then_reuses_the_kitty_probe_cache` above proves
/// *does* get skipped outside a multiplexer, and showing it does not get
/// skipped here.
#[test]
fn tmux_sessions_bypass_the_kitty_probe_cache() {
    let repo = fixture::basic_repo();
    let state_home = tempfile::tempdir().expect("tempdir for a shared $XDG_STATE_HOME");
    let cache_path = state_home.path().join("katamari").join("kitty-probe.json");
    // A real tmux `$TMUX` value's shape (socket path, pid, window); only
    // "non-empty" matters to `probe_cache::multiplexed_from_env`, but a
    // realistic value keeps this test honest about what it's simulating.
    let tmux_env = vec![(
        std::ffi::OsString::from("TMUX"),
        std::ffi::OsString::from("/tmp/tmux-1000/default,12345,0"),
    )];

    // First launch, `$TMUX` set: the real probe still answers (this
    // harness's default `KittyMode::Supported`), but `enable_kitty_keyboard_
    // protocol` must not persist that answer — see this test's own docs.
    {
        let mut h = Harness::spawn(
            repo.path(),
            SpawnOptions {
                state_home: Some(state_home.path().to_path_buf()),
                extra_env: tmux_env.clone(),
                ..Default::default()
            },
        );
        h.send(Key::Char('q'));
        let status = h.wait_exit(Duration::from_secs(5));
        assert!(
            status.success(),
            "first (tmux) launch should exit 0, got {status:?}"
        );
    }
    assert!(
        !cache_path.exists(),
        "a multiplexed launch must never write the kitty probe cache \
         (fingerprint has no terminal identity left to key on): found {}",
        cache_path.display()
    );

    // Second launch, same state home and same `$TMUX` value — if a verdict
    // *had* been written above (or if `enable_kitty_keyboard_protocol`
    // wrongly trusted the fingerprint despite never seeing a write), this
    // launch could skip the probe. It must not: the withheld reply below
    // must actually be waited on, proven the same relative way the
    // cache-hit test above proves the opposite — this launch must take
    // meaningfully *longer* than a cache-hit would, not shorter.
    let long_delay = Duration::from_secs(4);
    let start = Instant::now();
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            state_home: Some(state_home.path().to_path_buf()),
            extra_env: tmux_env,
            probe_reply_delay: Some(long_delay),
            ..Default::default()
        },
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed >= long_delay / 2,
        "a multiplexed launch must always re-probe rather than ever trust a \
         cached verdict; ready in {elapsed:?} despite a {long_delay:?} \
         withheld reply"
    );
    assert!(
        h.screen_contents().contains("README.md"),
        "the real diff view must still render once the withheld reply lands"
    );
    assert!(
        !cache_path.exists(),
        "the second multiplexed launch must not have written the cache \
         either: found {}",
        cache_path.display()
    );

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(
        status.success(),
        "second (tmux) launch should exit 0, got {status:?}"
    );
}

// ---- issue #13's emacs preset: M-f/M-b over real wire bytes ---------------

/// Issue #13's emacs preset binds `M-f`/`M-b` to `NextSymbol`/`PrevSymbol`
/// (real emacs `forward-word`/`backward-word`, repurposed here for the
/// active symbol — see `keymap::emacs_preset`'s own doc comment for why).
/// `keymap::tests::emacs_meta_f_and_meta_b_resolve_to_next_and_prev_symbol`
/// already proves the binding table entry is correct against a hand-built
/// `KeyEvent`; this proves the same claim through real bytes crossterm has
/// to decode — a bare `ESC` immediately followed by `f`/`b` under the
/// legacy fallback, or the kitty CSI-u modified form once the protocol has
/// negotiated (see `Key::AltChar`'s docs) — two genuinely different parsing
/// paths through crossterm, so this runs once per `KittyMode` rather than
/// trusting one to stand in for the other, the same reasoning
/// `kitty_supported`/`kitty_unsupported` above already follow for
/// `M-Left`/`M-Right`.
fn emacs_meta_f_and_meta_b_move_the_active_symbol(kitty_mode: KittyMode) {
    let repo = fixture::basic_repo();
    // Opt into the emacs preset the same way `tests/e2e/doctor.rs`/
    // `mouse.rs`/`update_check.rs` already write ad hoc `.katamari/config.toml`
    // fixtures: directly, rather than adding a new `fixture::` constructor
    // just one test in this file needs. Committed immediately (unlike those
    // other tests, which never need a deterministic bottom row) so it
    // doesn't itself show up as a fourth working-tree diff entry ahead of
    // `basic_repo`'s own three — `Action::Bottom` below needs the diff's
    // real last row to land on `todo.txt`'s pre-existing content exactly as
    // `basic_repo` wrote it, not shift onto this config write instead.
    std::fs::create_dir_all(repo.path().join(".katamari")).unwrap();
    std::fs::write(
        repo.path().join(".katamari").join("config.toml"),
        "keymap = \"emacs\"\n",
    )
    .unwrap();
    fixture::git(repo.path(), &["add", ".katamari/config.toml"]);
    fixture::git(repo.path(), &["commit", "-q", "-m", "enable emacs keymap"]);
    let mut h = Harness::spawn(
        repo.path(),
        SpawnOptions {
            kitty_mode,
            ..Default::default()
        },
    );
    h.wait_for_text("README.md");

    // The status bar's own "· {cursor+1}/{total}" position indicator
    // (`ui::status_bar`) is the only reliable witness that `Action::Bottom`
    // actually fired below: `basic_repo`'s whole diff is short enough to
    // render in full on this harness's 30-row terminal regardless of
    // cursor position, so waiting for any particular line's *text* to
    // appear would prove nothing about whether the cursor moved at all.
    const POSITION_MARKER: &str = "\u{b7} 1/";
    // `wait_for_text("README.md")` above only proves the diff pane's own
    // content painted, not that the (separately positioned) status bar's
    // own frame landed alongside it — wait for the exact substring this
    // parses below, rather than assuming it rode in on the same frame.
    h.wait_for_text(POSITION_MARKER);
    let initial = h.screen_contents();
    let digits_start = initial
        .find(POSITION_MARKER)
        .map(|i| i + POSITION_MARKER.len())
        .expect("initial cursor position must render as \"· 1/N\"");
    let total: usize = initial[digits_start..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .expect("row count after \"· 1/\" must be a plain integer");

    // `M->` is this preset's own `Action::Bottom` (real emacs
    // `end-of-buffer`) — lands the cursor on the diff's last row,
    // `todo.txt`'s own single `Add` line ("- write more fixtures"), a real
    // content row with several word-like symbols to cycle through, and
    // (per `App::update`'s "cursor moved" tail) resets `active_symbol` to 0
    // there. Reached this way rather than vim's `j`/`G`, which this preset
    // doesn't bind at all.
    h.send(Key::AltChar('>'));
    h.wait_for_text(&format!("\u{b7} {total}/{total}"));
    let before = h.with_screen(underlined_cells);
    assert!(
        !before.is_empty(),
        "the bottom row must have a real active symbol to cycle from; screen:\n{}",
        h.screen_contents()
    );

    // The actual payoff: real Alt-f bytes on the wire must reach
    // `Action::NextSymbol`, moving the underline off `active_symbol == 0`.
    h.send(Key::AltChar('f'));
    h.wait_until(Duration::from_secs(2), |s| {
        underlined_cells(s) != before && !underlined_cells(s).is_empty()
    });
    let after_forward = h.with_screen(underlined_cells);
    assert_ne!(
        after_forward, before,
        "M-f must have actually moved the active symbol"
    );

    // And real Alt-b bytes must reach `Action::PrevSymbol`, moving it back
    // to exactly where it started — proof this is real cycling, not some
    // other action that happened to change the screen.
    h.send(Key::AltChar('b'));
    h.wait_until(Duration::from_secs(2), |s| underlined_cells(s) == before);

    h.send(Key::Char('q'));
    let status = h.wait_exit(Duration::from_secs(5));
    assert!(status.success(), "ktmr should exit 0 on q, got {status:?}");
}

#[test]
fn emacs_meta_f_and_meta_b_move_the_active_symbol_over_kitty_csi_u_bytes() {
    emacs_meta_f_and_meta_b_move_the_active_symbol(KittyMode::Supported);
}

#[test]
fn emacs_meta_f_and_meta_b_move_the_active_symbol_over_legacy_escape_bytes() {
    emacs_meta_f_and_meta_b_move_the_active_symbol(KittyMode::Unsupported);
}
