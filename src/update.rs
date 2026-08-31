//! Background "a newer katamari is out" check, following the gh CLI /
//! npm `update-notifier` pattern: never let the check itself slow a session
//! down or break offline use. Concretely, that means every display surface
//! ([`on_startup`]'s status-bar text, [`print_exit_notice`]'s stderr line)
//! reads only a small cached JSON file this module wrote on some *previous*
//! run — never a live request — and the one thing that *does* talk to the
//! network ([`refresh_cache`]) runs on a detached background thread that
//! nothing ever joins, so a slow or unreachable GitHub API can, at worst,
//! make that thread outlive the process; it can never make the process wait
//! on it. [`crate::ui::run`] is the only caller, once per TUI session (see
//! its docs for why headless/plumbing subcommands like `ktmr comments` or
//! `--dump` never reach it at all).
//!
//! This module also owns the *upgrade-command* half of the story —
//! [`detect_upgrade_command`], shared with `ktmr self-update`
//! (`main.rs::run_self_update`) so the two never drift: this module decides
//! *whether* a receipt-managed self-update is possible (a cheap fs check,
//! see [`has_install_receipt`]) without depending on the `axoupdater` crate
//! itself, and `run_self_update` is what actually drives that crate to
//! perform one.
//!
//! The cache lives under `$XDG_STATE_HOME/katamari/update-check.json`
//! (falling back to `~/.local/state/katamari/` — see [`state_dir_from_env`]),
//! deliberately separate from [`crate::lsp::install::prefix_dir`]'s
//! `$XDG_DATA_HOME/katamari/servers`: a language server binary is *data* a
//! session depends on to function; this file is disposable *state* about
//! when katamari last phoned home, safe to delete any time with nothing
//! lost but a redundant network round trip. The two helpers share no code —
//! `prefix_dir`'s existing shape (env-injectable pure function + thin
//! real-env wrapper) is simply mirrored here, since the two XDG bases they
//! read are different environment variables serving different purposes, and
//! neither existing call site was written expecting a shared abstraction.

use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The app name cargo-dist's shell installer writes into the install
/// receipt (`source.app_name`/`name` in `katamari-installer.sh`'s
/// `RECEIPT` template) and the name a receipt file itself is stamped
/// with (`<app_name>-receipt.json`) — the package name, not either `[[bin]]`
/// name (`katamari`, not `ktmr`). Shared between [`has_install_receipt`]
/// here and `main.rs::run_self_update`'s `AxoUpdater::new_for`, so the two
/// can never name-mismatch and silently look at different receipts.
pub(crate) const APP_NAME: &str = "katamari";

const RELEASES_URL: &str = "https://api.github.com/repos/isamisushi/katamari/releases/latest";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a cached check result is trusted before a session bothers
/// spawning a background refresh — see [`is_stale`]. Chosen to match gh
/// CLI's own interval: frequent enough that a reviewer hears about a new
/// release within a workday, rare enough that `ktmr` run in a loop (a
/// script, a live-refresh session restarted repeatedly) never spends it on
/// more than one request a day.
const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

/// A newer release than the one running now — the only thing either display
/// surface needs to know. Carrying just the version string (not the whole
/// [`Cache`]) keeps [`on_startup`]'s return type honest about what's
/// actually used downstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailableUpdate {
    pub latest_version: String,
}

/// The state file's on-disk shape — see the module docs for its path.
/// `last_checked` is retried-on-failure-too (see [`refresh_cache`]'s docs):
/// an offline machine gets asked again once a day, not once a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Cache {
    last_checked: u64,
    latest_version: String,
}

/// Called once per TUI session, by [`crate::ui::run`]. Does two independent
/// things with the on-disk cache: kicks off a background refresh if it's
/// stale or missing (fire-and-forget — see the module docs), and returns
/// whatever *that same, not-yet-refreshed* cache says about this session,
/// for [`crate::ui::run`] to fold into its startup status note. The two
/// never interact: a refresh that finishes mid-session changes nothing about
/// what this session already decided to show — only the *next* run sees it.
///
/// `enabled` is `[update] check` (see [`crate::config::Config`]) — `false`
/// skips both the background request and ever reading or writing the cache
/// file, the complete opt-out the config docs promise.
pub fn on_startup(enabled: bool) -> Option<AvailableUpdate> {
    if !enabled {
        return None;
    }
    let path = state_file_path();
    let cached = read_cache(&path);
    let now = now_unix();
    let should_refresh = match &cached {
        Some(c) => is_stale(c.last_checked, now),
        None => true,
    };
    if should_refresh {
        std::thread::spawn(move || refresh_cache(&path));
    }
    available_update(cached.as_ref(), env!("CARGO_PKG_VERSION"))
}

/// The pure decision [`on_startup`]'s result boils down to: `cache`'s
/// `latest_version` only counts as "available" when it parses as a newer
/// version than `current` (see [`compare_versions`] for what "parses"
/// covers) — a missing cache, an unparseable one, or one that's equal to or
/// older than `current` all mean nothing to show, not an error.
fn available_update(cache: Option<&Cache>, current: &str) -> Option<AvailableUpdate> {
    let cache = cache?;
    match compare_versions(&cache.latest_version, current) {
        Some(Ordering::Greater) => Some(AvailableUpdate {
            latest_version: cache.latest_version.clone(),
        }),
        _ => None,
    }
}

/// The status-bar text for `update` — folded into [`crate::ui::run`]'s
/// startup status note the same way a warm-up cap or a failed watcher gets
/// there, so it shares that slot's "once, at startup, superseded by
/// anything more urgent" behavior rather than needing its own dismissal
/// mechanism.
pub fn status_bar_notice(update: &AvailableUpdate) -> String {
    upgrade_message(&update.latest_version, &detect_upgrade_command())
}

/// Prints the same message [`status_bar_notice`] shows, to stderr, only if
/// stderr is a real terminal (never into a pipe or a redirected log — this
/// is a one-time human hint, not output a script should have to filter) and
/// only if there's an update to report. [`crate::ui::run`] calls this after
/// the terminal is already restored, on a normal (non-error) exit.
pub fn print_exit_notice(update: Option<&AvailableUpdate>) {
    use std::io::IsTerminal;
    let Some(update) = update else { return };
    if !io::stderr().is_terminal() {
        return;
    }
    eprintln!(
        "{}",
        upgrade_message(&update.latest_version, &detect_upgrade_command())
    );
}

fn upgrade_message(latest_version: &str, command: &str) -> String {
    format!(
        "katamari v{latest_version} is available (you have v{}) — {command}",
        env!("CARGO_PKG_VERSION")
    )
}

// --- Background refresh -----------------------------------------------

/// The only network call in this module, and the only thing [`on_startup`]
/// ever spawns a thread for. All failure — a timeout, no network, a
/// malformed response, a write that couldn't land — is silent by design
/// (see the module docs): this runs unattended on a background thread with
/// no channel back to the session that spawned it, so there is nowhere to
/// report an error *to* even if this raised one. `last_checked` is written
/// unconditionally, success or failure, so a machine with no network access
/// gets retried once a day (per [`STALE_AFTER`]), not on every single run —
/// the same call gh CLI's update notifier makes, and the right one here:
/// silently retrying every invocation would mean an offline user pays this
/// request's ~3s timeout on every `ktmr diff`.
fn refresh_cache(path: &Path) {
    let now = now_unix();
    // A failed fetch keeps whatever version was already cached (if any)
    // rather than blanking it out — a transient network hiccup shouldn't
    // make a real pending update disappear from the next session's display.
    let previous_latest = read_cache(path).map(|c| c.latest_version);
    let latest_version = fetch_latest_release_tag()
        .or(previous_latest)
        .unwrap_or_default();
    let cache = Cache {
        last_checked: now,
        latest_version,
    };
    let _ = write_cache_atomic(path, &cache);
}

/// `GET`s the GitHub releases API for the pinned repo and pulls `tag_name`
/// out of the JSON body, stripping a leading `v` — `None` on any failure at
/// any step (network, non-2xx, malformed JSON, missing/non-string field),
/// collapsed into one outcome since [`refresh_cache`] treats every failure
/// identically. A `User-Agent` header is set because GitHub's API rejects
/// requests without one; the timeout keeps a slow/unreachable API from
/// leaving this background thread (harmless, but not free) running
/// indefinitely.
fn fetch_latest_release_tag() -> Option<String> {
    let response = ureq::get(RELEASES_URL)
        .set(
            "User-Agent",
            &format!("katamari/{}", env!("CARGO_PKG_VERSION")),
        )
        .timeout(NETWORK_TIMEOUT)
        .call()
        .ok()?;
    let body = response.into_string().ok()?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let tag = value.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').to_owned())
}

// --- Version comparison -------------------------------------------------

/// Parses a `v`-prefixed-or-not `major.minor.patch` tag into a comparable
/// triple, ignoring anything after the patch number (a prerelease suffix
/// like `-beta.1` or build metadata like `+build5`) — good enough for "is
/// there a newer release," which is all this module needs, without pulling
/// in a full semver dependency for it (see the milestone task's "pinned
/// tiny comparison function, not a new dependency"). Anything that doesn't
/// have at least three dot-separated, numeric-leading components — a
/// malformed tag, a non-version string, an empty one — yields `None` rather
/// than a wrong guess, so a botched release tag can only ever make this
/// module do nothing, never show a bogus "update available."
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_field = parts.next()?;
    let patch_digits: String = patch_field
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if patch_digits.is_empty() {
        return None;
    }
    let patch = patch_digits.parse().ok()?;
    Some((major, minor, patch))
}

/// `None` if either side fails to parse (see [`parse_version`]) — the
/// caller ([`available_update`]) treats that identically to "not newer,"
/// which is the only safe default for a tag this module doesn't understand.
fn compare_versions(a: &str, b: &str) -> Option<Ordering> {
    Some(parse_version(a)?.cmp(&parse_version(b)?))
}

// --- Upgrade command detection ------------------------------------------

/// The exact command to hand a reviewer for the install this binary is
/// running from, inferred from `exe_path` (expected already canonicalized —
/// see [`detect_upgrade_command`]) and `has_receipt` (see
/// [`has_install_receipt`]). Checked in an order that's about correctness,
/// not convenience: a Homebrew Cellar path gets `brew upgrade` and a
/// `~/.cargo/bin` path gets `cargo install --git ...` *before* the receipt
/// check ever runs, because both package managers own that binary — an
/// `axoupdater`-driven self-update would fight brew's Cellar bookkeeping (or
/// silently no-op against a `cargo install` binary axoupdater doesn't
/// recognize) rather than genuinely update it, so a receipt happening to
/// exist alongside one of those installs must never win. Only once neither
/// matches does a receipt get to suggest `ktmr self-update`; anything left —
/// a manually downloaded release binary with no receipt, a distro package, a
/// source build run in place — gets pointed at the releases page rather than
/// guessed at, since no single command is honest for all of those.
fn upgrade_command(exe_path: &Path, home: Option<&Path>, has_receipt: bool) -> String {
    if exe_path.to_string_lossy().contains("/Cellar/") {
        return "brew upgrade katamari".to_owned();
    }
    if let Some(home) = home
        && exe_path.starts_with(home.join(".cargo").join("bin"))
    {
        return "cargo install --git https://github.com/isamisushi/katamari".to_owned();
    }
    if has_receipt {
        return "ktmr self-update".to_owned();
    }
    "https://github.com/isamisushi/katamari/releases".to_owned()
}

/// [`upgrade_command`] against the real running process: `current_exe`
/// canonicalized (so a `~/.cargo/bin/ktmr` that's actually a symlink, or a
/// relative launch path, resolves to the real installed location before the
/// `/Cellar/`/`.cargo/bin` checks run), the real `$HOME`, and a real
/// [`has_install_receipt`] check against `$XDG_CONFIG_HOME`/`$HOME`. Falls
/// back to the releases-page message if every lookup fails — the same
/// "don't guess" default [`upgrade_command`] uses for a path it doesn't
/// recognize. `pub(crate)` so `main.rs::run_self_update` can reuse this
/// exact detection for its own no-receipt guidance, rather than
/// re-deriving it.
pub(crate) fn detect_upgrade_command() -> String {
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .unwrap_or_default();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg_config_home = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let has_receipt = has_install_receipt(APP_NAME, xdg_config_home.as_deref(), home.as_deref());
    upgrade_command(&exe, home.as_deref(), has_receipt)
}

// --- Install receipt detection ------------------------------------------

/// The cargo-dist install receipt path for `app_name`: the one location
/// `katamari-installer.sh` (see its `RECEIPT_HOME` variable) ever writes
/// one to, and the first location `axoupdater`'s own `load_receipt` checks
/// (`$XDG_CONFIG_HOME/<app_name>` when set and non-empty, else
/// `$HOME/.config/<app_name>`) — kept as a plain path computation, no I/O,
/// so a test can hand it a fabricated pair without touching the real
/// filesystem or `axoupdater` needing to be involved at all.
fn receipt_path(
    app_name: &str,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let config_home = match xdg_config_home {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => home?.join(".config"),
    };
    Some(
        config_home
            .join(app_name)
            .join(format!("{app_name}-receipt.json")),
    )
}

/// Whether a cargo-dist install receipt for `app_name` exists at
/// [`receipt_path`] — the one bit of actual I/O in this pair (a plain
/// `Path::exists`, no network), which is what lets
/// [`detect_upgrade_command`] point a shell-installer install at `ktmr
/// self-update` instead of the generic releases-page fallback. Doesn't
/// parse or validate the receipt's contents — that's `axoupdater`'s
/// `load_receipt`'s job, in `run_self_update` itself; a corrupt receipt
/// here still means "try `ktmr self-update`," and that command will report
/// the real reason it can't proceed.
fn has_install_receipt(
    app_name: &str,
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> bool {
    receipt_path(app_name, xdg_config_home, home).is_some_and(|p| p.exists())
}

// --- Staleness -----------------------------------------------------------

/// Whether a cache last refreshed at `last_checked_unix` (seconds since the
/// epoch) is due for another look as of `now_unix` — both plain `u64`
/// parameters rather than a live clock read internally, so tests exercise
/// this with fabricated timestamps directly (mirroring
/// [`crate::watch::debounce::Debounce`]'s "push time reads to the caller"
/// pattern; the one production call site, [`on_startup`], is the only place
/// that ever hands this a real [`now_unix`]).
fn is_stale(last_checked_unix: u64, now_unix: u64) -> bool {
    now_unix.saturating_sub(last_checked_unix) >= STALE_AFTER.as_secs()
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// --- Cache I/O -------------------------------------------------------------

/// `None` for a missing file (never checked yet) and, deliberately, for one
/// that fails to parse — a state file is disposable (see the module docs),
/// so a corrupt one degrades to "never checked" instead of erroring, the
/// same graceful-degradation rule [`crate::config`] applies to a bad config
/// file.
fn read_cache(path: &Path) -> Option<Cache> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

/// Writes `cache` to `path` via a temp-file-then-rename in the same
/// directory, so a reader (this module's own [`read_cache`], run from a
/// concurrent `ktmr` invocation) never observes a partially-written file —
/// the same atomicity [`crate::lsp::install::atomic_write_executable`] gets
/// for a downloaded server binary, applied here to a much smaller payload.
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

/// `$XDG_STATE_HOME/katamari/update-check.json`, or
/// `~/.local/state/katamari/update-check.json` when unset — see the module
/// docs for why this is a separate directory from
/// [`crate::lsp::install::prefix_dir`].
fn state_file_path() -> PathBuf {
    state_dir().join("update-check.json")
}

/// `$XDG_STATE_HOME/katamari`, or `~/.local/state/katamari` when unset.
/// Public (unlike [`state_file_path`]) because [`crate::lsp::adapter`]'s
/// jdtls per-workspace index directories belong under this same state root
/// — a project's language-server index is exactly the kind of large,
/// disposable, re-buildable-on-demand cache this directory is for, same as
/// this module's own update-check cache — and the crate should have exactly
/// one place that knows the `$XDG_STATE_HOME` fallback rule rather than two
/// copies drifting apart.
pub fn state_dir() -> PathBuf {
    state_dir_from_env(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
}

fn state_dir_from_env(xdg_state_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let base = match (xdg_state_home, home) {
        (Some(xdg), _) if !xdg.is_empty() => PathBuf::from(xdg),
        (_, Some(home)) if !home.is_empty() => PathBuf::from(home).join(".local").join("state"),
        // No `$HOME` at all (a bare container, a stripped CI env): fall
        // back to the tempdir rather than `.` — a relative fallback would
        // plant `./.local/state/katamari/` inside whatever repository is
        // being reviewed, and state written where the litter lands beats
        // state written into someone's project.
        _ => std::env::temp_dir().join(".local").join("state"),
    };
    base.join("katamari")
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- version comparison ---------------------------------------------

    #[test]
    fn newer_remote_version_compares_greater() {
        assert_eq!(compare_versions("1.3.0", "1.2.9"), Some(Ordering::Greater));
    }

    #[test]
    fn older_remote_version_compares_less() {
        assert_eq!(compare_versions("1.0.0", "1.2.0"), Some(Ordering::Less));
    }

    #[test]
    fn equal_versions_compare_equal() {
        assert_eq!(compare_versions("2.4.1", "2.4.1"), Some(Ordering::Equal));
    }

    #[test]
    fn a_leading_v_is_stripped_on_either_side() {
        assert_eq!(compare_versions("v1.3.0", "1.2.9"), Some(Ordering::Greater));
        assert_eq!(compare_versions("1.3.0", "v1.2.9"), Some(Ordering::Greater));
    }

    #[test]
    fn prerelease_suffixes_are_ignored_past_the_patch_number() {
        assert_eq!(
            compare_versions("1.3.0-beta.1", "1.2.9"),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn malformed_tags_are_ignored_rather_than_mis_compared() {
        assert_eq!(compare_versions("not-a-version", "1.2.9"), None);
        assert_eq!(compare_versions("1.2.9", "not-a-version"), None);
        assert_eq!(compare_versions("1.2", "1.2.9"), None, "missing patch");
        assert_eq!(compare_versions("", "1.2.9"), None);
    }

    // --- available_update (display-gating pure decision) -----------------

    fn cache(latest_version: &str) -> Cache {
        Cache {
            last_checked: 0,
            latest_version: latest_version.to_owned(),
        }
    }

    #[test]
    fn no_cache_means_no_notice() {
        assert_eq!(available_update(None, "1.0.0"), None);
    }

    #[test]
    fn a_newer_cached_version_produces_a_notice() {
        assert_eq!(
            available_update(Some(&cache("2.0.0")), "1.0.0"),
            Some(AvailableUpdate {
                latest_version: "2.0.0".to_owned()
            })
        );
    }

    #[test]
    fn an_equal_or_older_cached_version_produces_no_notice() {
        assert_eq!(available_update(Some(&cache("1.0.0")), "1.0.0"), None);
        assert_eq!(available_update(Some(&cache("0.9.0")), "1.0.0"), None);
    }

    #[test]
    fn a_malformed_cached_version_produces_no_notice() {
        assert_eq!(available_update(Some(&cache("garbage")), "1.0.0"), None);
    }

    // --- staleness ---------------------------------------------------------

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn fresh_cache_is_not_stale() {
        assert!(!is_stale(1_000, 1_000 + DAY - 1));
    }

    #[test]
    fn a_cache_exactly_a_day_old_is_stale() {
        assert!(is_stale(1_000, 1_000 + DAY));
    }

    #[test]
    fn a_much_older_cache_is_stale() {
        assert!(is_stale(0, 10 * DAY));
    }

    // --- upgrade command detection -----------------------------------------

    #[test]
    fn a_cellar_path_gets_brew_upgrade() {
        let exe = Path::new("/opt/homebrew/Cellar/katamari/0.1.0/bin/ktmr");
        assert_eq!(upgrade_command(exe, None, false), "brew upgrade katamari");
    }

    #[test]
    fn a_cargo_bin_path_gets_cargo_install_git() {
        let home = Path::new("/home/someone");
        let exe = home.join(".cargo").join("bin").join("ktmr");
        assert_eq!(
            upgrade_command(&exe, Some(home), false),
            "cargo install --git https://github.com/isamisushi/katamari"
        );
    }

    #[test]
    fn any_other_path_points_at_the_releases_page() {
        let exe = Path::new("/usr/local/bin/ktmr");
        assert_eq!(
            upgrade_command(exe, Some(Path::new("/home/someone")), false),
            "https://github.com/isamisushi/katamari/releases"
        );
    }

    #[test]
    fn a_cargo_bin_path_without_a_known_home_falls_back_to_releases() {
        // `home` unavailable (e.g. `$HOME` unset) means the `.cargo/bin`
        // check can't run at all — falls back rather than guessing.
        let exe = Path::new("/home/someone/.cargo/bin/ktmr");
        assert_eq!(
            upgrade_command(exe, None, false),
            "https://github.com/isamisushi/katamari/releases"
        );
    }

    #[test]
    fn a_receipt_present_with_no_package_manager_match_gets_self_update() {
        let exe = Path::new("/usr/local/bin/ktmr");
        assert_eq!(
            upgrade_command(exe, Some(Path::new("/home/someone")), true),
            "ktmr self-update"
        );
    }

    #[test]
    fn a_cellar_path_wins_over_a_present_receipt() {
        // A brew-managed binary must never be told to self-update — brew
        // owns that Cellar path and axoupdater doesn't know about it.
        let exe = Path::new("/opt/homebrew/Cellar/katamari/0.1.0/bin/ktmr");
        assert_eq!(upgrade_command(exe, None, true), "brew upgrade katamari");
    }

    #[test]
    fn a_cargo_bin_path_wins_over_a_present_receipt() {
        let home = Path::new("/home/someone");
        let exe = home.join(".cargo").join("bin").join("ktmr");
        assert_eq!(
            upgrade_command(&exe, Some(home), true),
            "cargo install --git https://github.com/isamisushi/katamari"
        );
    }

    #[test]
    fn no_receipt_and_no_package_manager_match_falls_back_to_releases() {
        let exe = Path::new("/usr/local/bin/ktmr");
        assert_eq!(
            upgrade_command(exe, Some(Path::new("/home/someone")), false),
            "https://github.com/isamisushi/katamari/releases"
        );
    }

    // --- install receipt detection ------------------------------------------

    #[test]
    fn receipt_path_prefers_xdg_config_home_when_set() {
        let path = receipt_path(
            "katamari",
            Some(Path::new("/custom/config")),
            Some(Path::new("/home/someone")),
        );
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/custom/config/katamari/katamari-receipt.json"
            ))
        );
    }

    #[test]
    fn receipt_path_falls_back_to_home_config_when_xdg_unset() {
        let path = receipt_path("katamari", None, Some(Path::new("/home/someone")));
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/home/someone/.config/katamari/katamari-receipt.json"
            ))
        );
    }

    #[test]
    fn receipt_path_ignores_an_empty_xdg_config_home() {
        let path = receipt_path(
            "katamari",
            Some(Path::new("")),
            Some(Path::new("/home/someone")),
        );
        assert_eq!(
            path,
            Some(PathBuf::from(
                "/home/someone/.config/katamari/katamari-receipt.json"
            ))
        );
    }

    #[test]
    fn receipt_path_is_none_without_xdg_or_home() {
        assert_eq!(receipt_path("katamari", None, None), None);
    }

    fn fixture_receipt_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir().join(format!("ktmr-receipt-test-{}-{n}", std::process::id()))
    }

    #[test]
    fn has_install_receipt_is_false_when_the_file_is_missing() {
        let xdg = fixture_receipt_dir();
        assert!(!has_install_receipt("katamari", Some(&xdg), None));
    }

    #[test]
    fn has_install_receipt_is_true_when_the_file_exists() {
        let xdg = fixture_receipt_dir();
        let dir = xdg.join("katamari");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("katamari-receipt.json"), "{}").unwrap();
        assert!(has_install_receipt("katamari", Some(&xdg), None));
    }

    // --- cache read/write round trip ----------------------------------------

    fn fixture_state_path() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("ktmr-update-test-{}-{n}", std::process::id()))
            .join("update-check.json")
    }

    #[test]
    fn write_then_read_round_trips() {
        let path = fixture_state_path();
        let written = Cache {
            last_checked: 12_345,
            latest_version: "9.9.9".to_owned(),
        };
        write_cache_atomic(&path, &written).unwrap();
        assert_eq!(read_cache(&path), Some(written));
    }

    #[test]
    fn read_cache_is_none_for_a_missing_file() {
        let path = fixture_state_path();
        assert_eq!(read_cache(&path), None);
    }

    #[test]
    fn read_cache_is_none_for_a_malformed_file() {
        let path = fixture_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(read_cache(&path), None);
    }

    #[test]
    fn write_cache_atomic_overwrites_a_previous_value() {
        let path = fixture_state_path();
        write_cache_atomic(
            &path,
            &Cache {
                last_checked: 1,
                latest_version: "1.0.0".to_owned(),
            },
        )
        .unwrap();
        write_cache_atomic(
            &path,
            &Cache {
                last_checked: 2,
                latest_version: "2.0.0".to_owned(),
            },
        )
        .unwrap();
        assert_eq!(
            read_cache(&path),
            Some(Cache {
                last_checked: 2,
                latest_version: "2.0.0".to_owned(),
            })
        );
    }

    // --- state_dir_from_env --------------------------------------------------

    #[test]
    fn state_dir_from_env_respects_xdg_state_home() {
        let path = state_dir_from_env(
            Some(OsString::from("/custom/state")),
            Some(OsString::from("/home/someone")),
        );
        assert_eq!(path, PathBuf::from("/custom/state/katamari"));
    }

    #[test]
    fn state_dir_from_env_falls_back_to_home_local_state_when_xdg_unset() {
        let path = state_dir_from_env(None, Some(OsString::from("/home/someone")));
        assert_eq!(path, PathBuf::from("/home/someone/.local/state/katamari"));
    }

    #[test]
    fn state_dir_from_env_ignores_an_empty_xdg_state_home() {
        let path = state_dir_from_env(Some(OsString::new()), Some(OsString::from("/home/x")));
        assert_eq!(path, PathBuf::from("/home/x/.local/state/katamari"));
    }

    #[test]
    fn state_dir_from_env_without_home_lands_in_the_tempdir_not_the_cwd() {
        let path = state_dir_from_env(None, None);
        assert!(
            path.is_absolute(),
            "a relative fallback would litter the reviewed repository: {}",
            path.display()
        );
        assert!(path.starts_with(std::env::temp_dir()));
    }

    // --- on_startup / status_bar_notice / print_exit_notice ------------------

    #[test]
    fn on_startup_returns_none_when_disabled() {
        // No network, no file I/O should even happen — this only checks the
        // return value, but `enabled = false` returning early before ever
        // touching `state_file_path()` is exactly what makes this safe to
        // run in the same process as every other test in this suite without
        // colliding on the real `$XDG_STATE_HOME`.
        assert_eq!(on_startup(false), None);
    }

    #[test]
    fn status_bar_notice_names_both_versions() {
        let update = AvailableUpdate {
            latest_version: "9.9.9".to_owned(),
        };
        let text = status_bar_notice(&update);
        assert!(text.contains("9.9.9"));
        assert!(text.contains(env!("CARGO_PKG_VERSION")));
    }

    #[test]
    fn print_exit_notice_is_a_no_op_with_no_update() {
        // Nothing to assert on stderr directly (see the milestone task's
        // note that the PTY harness mixes stdout/stderr) — this just proves
        // the `None` short-circuit never reaches the tty check or a format
        // call that could panic.
        print_exit_notice(None);
    }

    // --- real-network smoke test, run manually only -------------------------

    /// Exercises [`fetch_latest_release_tag`] against the real GitHub API.
    /// Deliberately `#[ignore]`d for the same reason
    /// `lsp::install`'s download tests are: `cargo test` must stay hermetic
    /// and network-free. Run it explicitly with `cargo test -- --ignored
    /// fetch_latest_release_tag_reaches_the_real_github_api`.
    #[test]
    #[ignore = "hits the real network (api.github.com); run manually"]
    fn fetch_latest_release_tag_reaches_the_real_github_api() {
        let tag = fetch_latest_release_tag();
        assert!(
            tag.is_some(),
            "expected a real tag_name back from the GitHub releases API"
        );
        let tag = tag.unwrap();
        assert!(
            parse_version(&tag).is_some(),
            "expected the real repo's latest release tag to parse as a version, got {tag:?}"
        );
    }
}
