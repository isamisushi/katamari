//! Caches [`crate::ui::enable_kitty_keyboard_protocol`]'s crossterm probe
//! result per terminal, so a terminal that never answers the kitty-keyboard-
//! protocol query (see `enable_kitty_keyboard_protocol`'s docs) pays
//! crossterm's up-to-2s synchronous wait (verified: `Duration::from_millis
//! (2000)` in `query_keyboard_enhancement_flags_raw`, crossterm 0.29.0's
//! `terminal/sys/unix.rs`) once per terminal, not on every single launch.
//! Before this existed, nothing remembered the answer between sessions —
//! `ktmr` re-asked and re-waited out the same terminal's silence on every
//! invocation, which is what the splash's second line (see
//! `render_startup_splash`) now exists to explain when it's actually about
//! to happen again.
//!
//! Mirrors [`crate::update`]'s state-file shape (env-injectable pure
//! functions + a thin real-env/real-fs wrapper, atomic temp-file-then-rename
//! writes, best-effort/degrade-to-"no cache" reads) rather than sharing code
//! with it — same reasoning as that module's own docs on why it and
//! `lsp::install::prefix_dir` don't share a helper: the two state files serve
//! different purposes and were never written expecting a shared abstraction.
//! Lives at `$XDG_STATE_HOME/katamari/kitty-probe.json`, beside
//! `update-check.json` in the same disposable-cache directory `ktmr reset
//! --cache` already removes wholesale (see [`cache_file_path`]).
//!
//! **Fingerprint.** The cache key is derived from three environment
//! variables — `TERM`, `TERM_PROGRAM`, `TERM_PROGRAM_VERSION` (each missing
//! or unset treated as an empty string; see [`fingerprint`]) — not a single
//! `TERM` alone, because `TERM_PROGRAM`/`TERM_PROGRAM_VERSION` are what
//! separate genuinely different emulators that all report the same `TERM`.
//! The one deliberate collision this leaves in place: every VTE-family
//! terminal (GNOME Terminal, most Linux distro default terminals, plenty of
//! embedded/CI terminals) reports `TERM=xterm-256color` with no
//! `TERM_PROGRAM` set at all, so they all fingerprint identically — but they
//! also all uniformly lack kitty keyboard protocol support today, so that
//! collision is harmless: every terminal sharing that fingerprint would have
//! probed to the same `false` verdict anyway.
//!
//! **Staleness.** There is no expiry and no revalidation — a verdict, once
//! cached for a fingerprint, is trusted forever. The one way this can go
//! stale is a user switching to a *different* terminal emulator that happens
//! to reuse an already-cached fingerprint (e.g. two VTE-family terminals, or
//! upgrading an emulator that changes `TERM_PROGRAM_VERSION` in a way that
//! doesn't affect kitty support) — a real but narrow case, and `ktmr reset
//! --cache` (which removes this file along with every other disposable
//! cache) is the escape hatch when it matters.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

/// One fingerprint's cached verdict. A struct (not a bare `bool`) so the
/// on-disk shape has room to grow — e.g. a future confidence note or probe
/// timestamp — without a breaking format change to every existing cache
/// file on a user's disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Entry {
    supported: bool,
}

/// The on-disk shape: `{"<fingerprint>": {"supported": true}, ...}`. A plain
/// map at the JSON top level (no wrapper struct) so `serde_json` serializes
/// and deserializes it directly — this cache never needs metadata alongside
/// the entries the way `update.rs`'s own `Cache` needs `last_checked`.
type Cache = HashMap<String, Entry>;

/// `$XDG_STATE_HOME/katamari/kitty-probe.json` — see the module docs for why
/// this sits beside [`crate::update::state_dir`]'s `update-check.json`
/// rather than getting its own top-level state directory.
pub(crate) fn cache_file_path() -> PathBuf {
    crate::update::state_dir().join("kitty-probe.json")
}

/// [`fingerprint`] applied to this process's real environment — the only
/// fingerprint `enable_kitty_keyboard_protocol` ever actually looks up or
/// records against. Kept separate from the pure [`fingerprint`] function so
/// a test can exercise the construction logic against fabricated env values
/// without touching this process's real environment (mirrors
/// [`crate::update::state_dir`]/`state_dir_from_env`'s split).
pub(crate) fn fingerprint_from_env() -> String {
    fingerprint(
        &env_var("TERM"),
        &env_var("TERM_PROGRAM"),
        &env_var("TERM_PROGRAM_VERSION"),
    )
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default()
}

/// Builds the cache key from the three env values that identify a terminal
/// emulator (see the module docs' **Fingerprint** section for why these
/// three and not just `TERM`, and why the VTE-family collision they leave in
/// place is harmless). Formatted as `key=value` pairs rather than a bare
/// joined string so a doctor report or a debug log can print the fingerprint
/// itself and have it read as self-explanatory, not as three opaque
/// pipe-separated fields.
fn fingerprint(term: &str, term_program: &str, term_program_version: &str) -> String {
    format!("term={term}|term_program={term_program}|term_program_version={term_program_version}")
}

/// The cached verdict for `fingerprint`, if any — `None` for a fingerprint
/// this terminal has never been recorded under, a missing cache file, or one
/// that fails to parse (corrupt/truncated/foreign JSON). All three degrade
/// identically to "no cache," exactly like `update.rs`'s own `read_cache`:
/// this file is disposable, so the caller
/// ([`crate::ui::enable_kitty_keyboard_protocol`]) just re-probes and
/// re-writes rather than erroring.
pub(crate) fn look_up(path: &Path, fingerprint: &str) -> Option<bool> {
    read_cache(path)?
        .get(fingerprint)
        .map(|entry| entry.supported)
}

/// Records `supported` for `fingerprint`, best-effort: a failure to read the
/// existing cache (missing/corrupt — starts from an empty map, same as
/// [`look_up`]'s degradation) or to write the updated one back (a read-only
/// state dir, a filesystem at capacity) is silently swallowed. There is
/// nowhere useful to report a caching failure to at this point in startup —
/// worst case, the next launch in this terminal probes again, exactly the
/// pre-cache behavior this module improves on, not a regression from it.
pub(crate) fn record(path: &Path, fingerprint: &str, supported: bool) {
    let mut cache = read_cache(path).unwrap_or_default();
    cache.insert(fingerprint.to_owned(), Entry { supported });
    let _ = write_cache_atomic(path, &cache);
}

fn read_cache(path: &Path) -> Option<Cache> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Temp-file-then-rename in the same directory, so a concurrent `ktmr`
/// invocation's [`look_up`] never observes a partially-written file — the
/// same atomicity `update.rs`'s own `write_cache_atomic` gets for its own
/// cache, applied here rather than shared with it (see the module docs).
fn write_cache_atomic(path: &Path, cache: &Cache) -> io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| io::Error::other(format!("{}: no parent directory", path.display())))?;
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::other(format!("{}: no file name", path.display())))?;
    let tmp_path = dir.join(format!(
        ".{}.tmp-{}",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    let json = serde_json::to_vec_pretty(cache).map_err(io::Error::other)?;
    std::fs::write(&tmp_path, json)?;
    std::fs::rename(&tmp_path, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- fingerprint construction -------------------------------------------

    #[test]
    fn fingerprint_encodes_all_three_fields_as_self_explanatory_pairs() {
        assert_eq!(
            fingerprint("xterm-256color", "", ""),
            "term=xterm-256color|term_program=|term_program_version="
        );
        assert_eq!(
            fingerprint("xterm-256color", "iTerm.app", "3.5.0"),
            "term=xterm-256color|term_program=iTerm.app|term_program_version=3.5.0"
        );
    }

    #[test]
    fn different_term_program_values_produce_different_fingerprints() {
        // Two emulators that both happen to report the same TERM must not
        // collide just because of that one field.
        let iterm = fingerprint("xterm-256color", "iTerm.app", "3.5.0");
        let wezterm = fingerprint("xterm-256color", "WezTerm", "20240203");
        assert_ne!(iterm, wezterm);
    }

    #[test]
    fn different_term_program_versions_produce_different_fingerprints() {
        let old = fingerprint("xterm-256color", "iTerm.app", "3.4.0");
        let new = fingerprint("xterm-256color", "iTerm.app", "3.5.0");
        assert_ne!(old, new);
    }

    #[test]
    fn vte_family_terminals_collide_on_the_same_fingerprint() {
        // The documented, deliberate collision: no TERM_PROGRAM at all is
        // what every plain VTE-based terminal reports, and they uniformly
        // lack kitty support, so sharing one fingerprint (and thus one
        // cached verdict) across them is harmless by construction, not an
        // oversight — see the module docs' Fingerprint section.
        let gnome_terminal = fingerprint("xterm-256color", "", "");
        let some_other_vte_terminal = fingerprint("xterm-256color", "", "");
        assert_eq!(gnome_terminal, some_other_vte_terminal);
    }

    // --- cache read/write round trip ----------------------------------------

    fn fixture_cache_path() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("ktmr-kitty-probe-test-{}-{n}", std::process::id()))
            .join("kitty-probe.json")
    }

    #[test]
    fn look_up_is_none_for_a_missing_file() {
        let path = fixture_cache_path();
        assert_eq!(
            look_up(
                &path,
                "term=xterm-256color|term_program=|term_program_version="
            ),
            None
        );
    }

    #[test]
    fn look_up_is_none_for_a_corrupt_file() {
        let path = fixture_cache_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(look_up(&path, "anything"), None);
    }

    #[test]
    fn look_up_is_none_for_a_fingerprint_never_recorded() {
        let path = fixture_cache_path();
        record(
            &path,
            "term=known|term_program=|term_program_version=",
            true,
        );
        assert_eq!(
            look_up(&path, "term=unknown|term_program=|term_program_version="),
            None
        );
    }

    #[test]
    fn record_then_look_up_round_trips_true() {
        let path = fixture_cache_path();
        let fp = "term=xterm-kitty|term_program=kitty|term_program_version=0.32.0";
        record(&path, fp, true);
        assert_eq!(look_up(&path, fp), Some(true));
    }

    #[test]
    fn record_then_look_up_round_trips_false() {
        let path = fixture_cache_path();
        let fp = "term=xterm-256color|term_program=|term_program_version=";
        record(&path, fp, false);
        assert_eq!(look_up(&path, fp), Some(false));
    }

    #[test]
    fn recording_a_second_fingerprint_preserves_the_first() {
        let path = fixture_cache_path();
        let a = "term=a|term_program=|term_program_version=";
        let b = "term=b|term_program=|term_program_version=";
        record(&path, a, true);
        record(&path, b, false);
        assert_eq!(look_up(&path, a), Some(true));
        assert_eq!(look_up(&path, b), Some(false));
    }

    #[test]
    fn recording_the_same_fingerprint_again_overwrites_the_verdict() {
        let path = fixture_cache_path();
        let fp = "term=x|term_program=|term_program_version=";
        record(&path, fp, false);
        record(&path, fp, true);
        assert_eq!(look_up(&path, fp), Some(true));
    }

    #[test]
    fn a_corrupt_existing_file_does_not_stop_record_from_writing_a_fresh_one() {
        // `record`'s own "start from an empty map on a bad read" degradation
        // (mirroring `look_up`'s) must not lose the write it was actually
        // asked to make.
        let path = fixture_cache_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ this is not valid json").unwrap();
        let fp = "term=x|term_program=|term_program_version=";
        record(&path, fp, true);
        assert_eq!(look_up(&path, fp), Some(true));
    }
}
