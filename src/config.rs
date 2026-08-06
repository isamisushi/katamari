//! User configuration: TOML files merged `defaults < ~/.config/katamari/config.toml
//! < <repo_root>/.katamari/config.toml`, loaded once at startup by every
//! entry point that launches the TUI (`main.rs`'s `run_diff`/`run_open`/
//! `run_timeline`). Two knobs from it — [`tab_width`] and
//! [`highlight_max_lines`] — are read from deep inside the rendering and LSP
//! request-dispatch call graphs (`diff::coords::ColumnMap`,
//! `ui::symbols::scan`, `ui::text`, `lsp::manager`, `ui::navigation`,
//! `ui::refs_panel`), places with no natural path back to whichever `App`/
//! `FileView` a session happens to be showing. Rather than thread a
//! `tab_width: usize` parameter through every one of those call sites for a
//! value that is, in practice, one process-wide rendering constant (the
//! same role a compile-time constant played before this milestone), they
//! read it through [`tab_width`]/[`highlight_max_lines`] — a pair of
//! `OnceLock`s installed once via [`install`], right after a session's
//! `Config` is loaded and before anything renders. Everything else config
//! touches (which keymap preset, LSP server overrides, the watch debounce)
//! has a natural, single construction site to receive it as an explicit
//! parameter instead, and does.

use crate::keymap::{self, Action, KeySeq};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

pub const DEFAULT_TAB_WIDTH: usize = 4;
pub const DEFAULT_HIGHLIGHT_MAX_LINES: usize = 5000;
pub const DEFAULT_DEBOUNCE_MS: u64 = 200;

/// Which built-in binding table `ui::run` builds its [`crate::keymap::Keymap`]
/// from — `[keys]` overrides (see [`apply_key_overrides`]) apply on top of
/// whichever preset this selects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeymapPreset {
    Vim,
    Emacs,
}

/// One `[lsp.servers.<lang>]` entry: overrides the command
/// [`crate::lsp::adapter::resolve_server`] would otherwise resolve for that
/// language, taking priority over every built-in lookup (PATH, project-local
/// installs, `rustup which`) — useful for a pinned server version, a wrapper
/// script, or a server this module doesn't know how to find on its own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ServerOverride {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
}

/// The fully resolved configuration for one session: defaults with every
/// merged file's present fields applied on top (see the module docs for the
/// merge order). Every field here has a value regardless of whether any
/// config file existed at all — a session with no `config.toml` anywhere
/// behaves exactly as M1-M6 did.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub keymap: KeymapPreset,
    /// `[keys]`: action name (see [`keymap::action_name`]) to key-sequence
    /// notation (see [`KeySeq::try_parse`]), applied over the selected
    /// preset by [`apply_key_overrides`].
    pub key_overrides: HashMap<String, String>,
    /// `[lsp.servers.<lang>]`, keyed by the same lowercase language name
    /// [`crate::lsp::adapter::Language::lsp_id`] reports (`"rust"`,
    /// `"typescript"`, `"python"`, `"go"`).
    pub lsp_servers: HashMap<String, ServerOverride>,
    /// `[lsp] auto_install` — whether [`crate::lsp::manager::LspManager`]
    /// may silently download/build a missing language server into
    /// katamari's own prefix (see [`crate::lsp::install`]) instead of just
    /// reporting it unavailable. Defaults to `true`, matching the
    /// VSCode/Zed-style "it just works" experience this exists for; `false`
    /// restores the pre-M8b behavior of a manual-install hint only.
    pub auto_install: bool,
    pub tab_width: usize,
    pub highlight_max_lines: usize,
    pub debounce_ms: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            keymap: KeymapPreset::Vim,
            key_overrides: HashMap::new(),
            lsp_servers: HashMap::new(),
            auto_install: true,
            tab_width: DEFAULT_TAB_WIDTH,
            highlight_max_lines: DEFAULT_HIGHLIGHT_MAX_LINES,
            debounce_ms: DEFAULT_DEBOUNCE_MS,
        }
    }
}

// --- Raw TOML shape -------------------------------------------------------
//
// A second, `Option`-everywhere set of types separate from `Config` itself:
// merging has to distinguish "this file didn't mention `tab_width`" from
// "this file set `tab_width` to its already-default value" — a plain
// `Config` (or a `Config` with `#[serde(default)]`) can't represent that
// distinction, since a missing field and a field equal to the default would
// deserialize identically. `RawFile` keeps every field optional so merging
// (see `merge_raw`) only ever overwrites what a file actually specified.

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
struct RawFile {
    keymap: Option<String>,
    keys: Option<HashMap<String, String>>,
    lsp: Option<RawLsp>,
    ui: Option<RawUi>,
    watch: Option<RawWatch>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawLsp {
    servers: HashMap<String, ServerOverride>,
    auto_install: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawUi {
    tab_width: Option<usize>,
    highlight_max_lines: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct RawWatch {
    debounce_ms: Option<u64>,
}

const TOP_LEVEL_KEYS: &[&str] = &["keymap", "keys", "lsp", "ui", "watch"];
const LSP_KEYS: &[&str] = &["servers", "auto_install"];
const SERVER_KEYS: &[&str] = &["command", "args"];
const UI_KEYS: &[&str] = &["tab_width", "highlight_max_lines"];
const WATCH_KEYS: &[&str] = &["debounce_ms"];

/// Merges `overlay`'s present fields onto `base`, in place — the field-level
/// (not whole-file) precedence rule the module docs describe: a file that
/// only sets `[ui] tab_width` doesn't reset `[watch] debounce_ms` back to
/// whatever the lower-priority file (or the built-in default) had it at.
/// Maps (`keys`, `lsp.servers`) merge key-by-key with `overlay` winning
/// per-key, rather than replacing the whole map, for the same reason.
fn merge_raw(base: &mut RawFile, overlay: RawFile) {
    if overlay.keymap.is_some() {
        base.keymap = overlay.keymap;
    }
    if let Some(overlay_keys) = overlay.keys {
        base.keys
            .get_or_insert_with(HashMap::new)
            .extend(overlay_keys);
    }
    if let Some(overlay_lsp) = overlay.lsp {
        let base_lsp = base.lsp.get_or_insert_with(RawLsp::default);
        base_lsp.servers.extend(overlay_lsp.servers);
        if overlay_lsp.auto_install.is_some() {
            base_lsp.auto_install = overlay_lsp.auto_install;
        }
    }
    if let Some(overlay_ui) = overlay.ui {
        let base_ui = base.ui.get_or_insert_with(RawUi::default);
        if overlay_ui.tab_width.is_some() {
            base_ui.tab_width = overlay_ui.tab_width;
        }
        if overlay_ui.highlight_max_lines.is_some() {
            base_ui.highlight_max_lines = overlay_ui.highlight_max_lines;
        }
    }
    if let Some(overlay_watch) = overlay.watch {
        let base_watch = base.watch.get_or_insert_with(RawWatch::default);
        if overlay_watch.debounce_ms.is_some() {
            base_watch.debounce_ms = overlay_watch.debounce_ms;
        }
    }
}

/// Loads and merges every config file that exists for `repo_root` (see the
/// module docs for the two paths and their precedence), warning to stderr
/// about any unrecognized key along the way rather than failing the whole
/// session over a typo or a stale field from an older katamari version — a
/// config file with one bad key is far more likely than one worth refusing
/// to start over. A missing file at either location is silently fine (most
/// sessions have neither); a present-but-unparseable file is reported the
/// same way an unknown key is, then treated as if it were absent.
pub fn load_merged(repo_root: &Path) -> Config {
    let mut raw = RawFile::default();
    if let Some(home_path) = home_config_path() {
        merge_from_file(&mut raw, &home_path);
    }
    let repo_path = repo_root.join(".katamari").join("config.toml");
    merge_from_file(&mut raw, &repo_path);
    finalize(raw)
}

fn merge_from_file(base: &mut RawFile, path: &Path) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return; // absent (or unreadable) config file: not an error, just nothing to merge
    };
    merge_raw(base, parse_and_warn(path, &text));
}

fn home_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("katamari")
            .join("config.toml"),
    )
}

/// Parses `text` (from `path`, used only to name it in a warning) as a
/// [`RawFile`], first scanning it for keys this module doesn't recognize
/// (see [`warn_unknown_keys`]) and then deserializing normally — serde
/// itself silently ignores unknown fields, which is right for *not
/// crashing* but wrong for *not saying anything*, hence the separate scan.
/// A file that fails to parse as TOML at all, or whose recognized fields
/// have the wrong shape (e.g. `tab_width = "four"`), is reported the same
/// way and treated as empty rather than propagated as an error — see
/// [`load_merged`]'s docs on why a bad file degrades instead of failing the
/// session.
fn parse_and_warn(path: &Path, text: &str) -> RawFile {
    // `toml::Value: FromStr` parses a single value *literal* (e.g. `"42"`),
    // not a whole document — `toml::Table` is the document-level parser
    // (a TOML file's top level is always a table of key/value pairs).
    let table: toml::Table = match text.parse() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("katamari: warning: {}: {e}", path.display());
            return RawFile::default();
        }
    };
    warn_unknown_keys(path, &table);
    match table.try_into() {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!("katamari: warning: {}: {e}", path.display());
            RawFile::default()
        }
    }
}

/// Prints one `katamari: warning: <path>: unknown config key '<dotted
/// path>'` line per key this module has never heard of, at every level this
/// config format has fixed keys for. `[keys]` is deliberately excluded: its
/// keys are action names, i.e. data this module validates at
/// [`apply_key_overrides`] time (against the live `Action` enum, with a
/// clear per-entry error), not schema to check here.
fn warn_unknown_keys(path: &Path, table: &toml::Table) {
    warn_extra(path, "", table, TOP_LEVEL_KEYS);
    if let Some(lsp) = table.get("lsp").and_then(toml::Value::as_table) {
        warn_extra(path, "lsp.", lsp, LSP_KEYS);
        if let Some(servers) = lsp.get("servers").and_then(toml::Value::as_table) {
            for (lang, server) in servers {
                if let Some(server_table) = server.as_table() {
                    warn_extra(
                        path,
                        &format!("lsp.servers.{lang}."),
                        server_table,
                        SERVER_KEYS,
                    );
                }
            }
        }
    }
    if let Some(ui) = table.get("ui").and_then(toml::Value::as_table) {
        warn_extra(path, "ui.", ui, UI_KEYS);
    }
    if let Some(watch) = table.get("watch").and_then(toml::Value::as_table) {
        warn_extra(path, "watch.", watch, WATCH_KEYS);
    }
}

fn warn_extra(
    path: &Path,
    prefix: &str,
    table: &toml::map::Map<String, toml::Value>,
    known: &[&str],
) {
    for key in table.keys() {
        if !known.contains(&key.as_str()) {
            eprintln!(
                "katamari: warning: {}: unknown config key `{prefix}{key}`",
                path.display()
            );
        }
    }
}

fn finalize(raw: RawFile) -> Config {
    let keymap = match raw.keymap.as_deref() {
        None | Some("vim") => KeymapPreset::Vim,
        Some("emacs") => KeymapPreset::Emacs,
        Some(other) => {
            eprintln!(
                "katamari: warning: unknown `keymap` preset {other:?} (expected \"vim\" or \"emacs\"); using vim"
            );
            KeymapPreset::Vim
        }
    };
    let ui = raw.ui.unwrap_or_default();
    let watch = raw.watch.unwrap_or_default();
    let lsp = raw.lsp.unwrap_or_default();
    Config {
        keymap,
        key_overrides: raw.keys.unwrap_or_default(),
        lsp_servers: lsp.servers,
        auto_install: lsp.auto_install.unwrap_or(true),
        tab_width: ui.tab_width.unwrap_or(DEFAULT_TAB_WIDTH),
        highlight_max_lines: ui
            .highlight_max_lines
            .unwrap_or(DEFAULT_HIGHLIGHT_MAX_LINES),
        debounce_ms: watch.debounce_ms.unwrap_or(DEFAULT_DEBOUNCE_MS),
    }
}

/// Rebinds each `overrides` entry's action to its given key sequence,
/// replacing that action's binding from `bindings` (a vim/emacs preset — see
/// [`crate::keymap::vim_preset`]/[`crate::keymap::emacs_preset`]) in place,
/// or appending a new binding if the action somehow wasn't already present.
/// Returns a clear, entry-naming error (rather than silently skipping) for
/// an unrecognized action name or a malformed key-sequence — a typo in a
/// config file that quietly did nothing would be far more confusing to
/// debug than a startup error pointing at exactly which `[keys]` line is
/// wrong.
pub fn apply_key_overrides(
    mut bindings: Vec<(KeySeq, Action)>,
    overrides: &HashMap<String, String>,
) -> Result<Vec<(KeySeq, Action)>, String> {
    for (name, notation) in overrides {
        let action = keymap::action_by_name(name)
            .ok_or_else(|| format!("[keys]: unrecognized action name `{name}`"))?;
        let seq = KeySeq::try_parse(notation)
            .map_err(|e| format!("[keys] {name} = {notation:?}: {e}"))?;
        match bindings.iter_mut().find(|(_, a)| *a == action) {
            Some(slot) => slot.0 = seq,
            None => bindings.push((seq, action)),
        }
    }
    Ok(bindings)
}

// --- Process-wide rendering knobs -----------------------------------------
//
// See the module docs for why these two are read through a global rather
// than threaded as parameters.

static TAB_WIDTH: OnceLock<usize> = OnceLock::new();
static HIGHLIGHT_MAX_LINES: OnceLock<usize> = OnceLock::new();

/// Installs `config`'s rendering knobs for [`tab_width`]/
/// [`highlight_max_lines`] to read — called exactly once per process, by
/// every entry point that launches the TUI, before the first frame renders.
/// A second call is a no-op (each `OnceLock` keeps its first value): nothing
/// in this program loads more than one `Config` per run, so this should
/// never happen outside of tests, several of which install their own values
/// to exercise a specific tab width — see this module's test-only
/// `install_for_test`.
pub fn install(config: &Config) {
    let _ = TAB_WIDTH.set(config.tab_width);
    let _ = HIGHLIGHT_MAX_LINES.set(config.highlight_max_lines);
}

/// The configured tab-stop width columns of raw text advance to when a
/// literal tab is expanded — see `ui::text::expand_tabs_in_spans` and
/// `diff::coords::ColumnMap`, the two places that must agree on this value
/// for the terminal's on-screen alignment and an LSP request's coordinates
/// to stay in sync. [`DEFAULT_TAB_WIDTH`] when nothing installed a
/// [`Config`] yet (every non-TUI code path — parsing, `--dump`, `ktmr
/// comments`).
pub fn tab_width() -> usize {
    *TAB_WIDTH.get().unwrap_or(&DEFAULT_TAB_WIDTH)
}

/// The changed-line threshold above which a diff file's syntax highlighting
/// is skipped in favor of plain styling (see `diff::DiffFile::skip_highlighting`)
/// — and, sharing the same threshold, the same file is excluded from LSP
/// warm-up's `didOpen` calls (see `ui::warm_up_root`'s docs).
pub fn highlight_max_lines() -> usize {
    *HIGHLIGHT_MAX_LINES
        .get()
        .unwrap_or(&DEFAULT_HIGHLIGHT_MAX_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A process-wide counter so each test writes its repo config into a
    /// distinct temp directory — `load_merged` also reads `$HOME`, which is
    /// shared across every test in this binary, so only the repo-level file
    /// (this fixture's own directory) is safe to vary per test without
    /// racing another test's `$HOME` mutation.
    static FIXTURE_SEQ: AtomicUsize = AtomicUsize::new(0);

    fn fixture_repo() -> PathBuf {
        let n = FIXTURE_SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("ktmr-config-test-{}-{n}", std::process::id()));
        std::fs::create_dir_all(dir.join(".katamari")).unwrap();
        dir
    }

    fn write_repo_config(repo_root: &Path, contents: &str) {
        std::fs::write(repo_root.join(".katamari").join("config.toml"), contents).unwrap();
    }

    #[test]
    fn absent_config_files_yield_plain_defaults() {
        let repo = fixture_repo();
        let config = load_merged(&repo);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn repo_config_overrides_a_single_field_without_resetting_the_rest() {
        let repo = fixture_repo();
        write_repo_config(&repo, "[ui]\ntab_width = 8\n");
        let config = load_merged(&repo);
        assert_eq!(config.tab_width, 8);
        // Nothing else in this file was set — every other field keeps its
        // built-in default rather than reverting the whole `[ui]` section.
        assert_eq!(config.highlight_max_lines, DEFAULT_HIGHLIGHT_MAX_LINES);
        assert_eq!(config.debounce_ms, DEFAULT_DEBOUNCE_MS);
    }

    #[test]
    fn repo_config_takes_precedence_over_home_config_field_by_field() {
        // Simulate the merge directly rather than mutating the real $HOME,
        // which every test in this binary shares: `merge_raw` is exactly
        // the function `load_merged` uses between the two files, so
        // exercising it directly proves the same precedence rule without
        // that shared, racy global.
        let mut base = RawFile {
            ui: Some(RawUi {
                tab_width: Some(8),
                highlight_max_lines: Some(1000),
            }),
            watch: Some(RawWatch {
                debounce_ms: Some(500),
            }),
            ..RawFile::default()
        };
        let overlay = RawFile {
            ui: Some(RawUi {
                tab_width: Some(2),
                highlight_max_lines: None,
            }),
            ..RawFile::default()
        };
        merge_raw(&mut base, overlay);
        let config = finalize(base);
        // The repo-level (`overlay`) value for `tab_width` won...
        assert_eq!(config.tab_width, 2);
        // ...but a field the repo file never mentioned keeps the home
        // file's value rather than falling back past it to the default.
        assert_eq!(config.highlight_max_lines, 1000);
        assert_eq!(config.debounce_ms, 500);
    }

    #[test]
    fn keys_and_lsp_servers_maps_merge_key_by_key() {
        let mut base = RawFile {
            keys: Some(HashMap::from([("quit".to_owned(), "q".to_owned())])),
            ..RawFile::default()
        };
        let overlay = RawFile {
            keys: Some(HashMap::from([("hover".to_owned(), "K".to_owned())])),
            ..RawFile::default()
        };
        merge_raw(&mut base, overlay);
        let keys = base.keys.unwrap();
        assert_eq!(keys.get("quit").map(String::as_str), Some("q"));
        assert_eq!(keys.get("hover").map(String::as_str), Some("K"));
    }

    #[test]
    fn unrecognized_top_level_keys_do_not_crash_and_fall_back_to_defaults() {
        let repo = fixture_repo();
        write_repo_config(&repo, "not_a_real_field = 42\n[ui]\ntab_width = 6\n");
        let config = load_merged(&repo);
        assert_eq!(config.tab_width, 6, "recognized fields still apply");
    }

    #[test]
    fn malformed_toml_falls_back_to_defaults_instead_of_panicking() {
        let repo = fixture_repo();
        write_repo_config(&repo, "this is not [ valid toml");
        let config = load_merged(&repo);
        assert_eq!(config, Config::default());
    }

    #[test]
    fn apply_key_overrides_rebinds_an_existing_action() {
        let bindings = keymap::vim_preset(false);
        let overrides = HashMap::from([("quit".to_owned(), "Z Z".to_owned())]);
        let rebound = apply_key_overrides(bindings, &overrides).unwrap();
        let quit_seq = rebound
            .iter()
            .find(|(_, a)| *a == Action::Quit)
            .map(|(seq, _)| format!("{seq:?}"));
        assert!(quit_seq.unwrap().contains("Char('Z')"));
    }

    #[test]
    fn apply_key_overrides_reports_an_unknown_action_name() {
        let overrides = HashMap::from([("not-a-real-action".to_owned(), "q".to_owned())]);
        let err = apply_key_overrides(keymap::vim_preset(false), &overrides).unwrap_err();
        assert!(err.contains("not-a-real-action"), "error was: {err}");
    }

    #[test]
    fn apply_key_overrides_reports_a_bad_key_sequence_naming_the_entry() {
        let overrides = HashMap::from([("quit".to_owned(), "NotAKey".to_owned())]);
        let err = apply_key_overrides(keymap::vim_preset(false), &overrides).unwrap_err();
        assert!(err.contains("quit"), "error should name the entry: {err}");
        assert!(
            err.contains("NotAKey"),
            "error should name the bad token: {err}"
        );
    }

    #[test]
    fn keymap_preset_parses_vim_and_emacs_and_warns_on_anything_else() {
        let repo = fixture_repo();
        write_repo_config(&repo, "keymap = \"emacs\"\n");
        assert_eq!(load_merged(&repo).keymap, KeymapPreset::Emacs);

        let repo2 = fixture_repo();
        write_repo_config(&repo2, "keymap = \"vim\"\n");
        assert_eq!(load_merged(&repo2).keymap, KeymapPreset::Vim);

        let repo3 = fixture_repo();
        write_repo_config(&repo3, "keymap = \"dvorak\"\n");
        assert_eq!(
            load_merged(&repo3).keymap,
            KeymapPreset::Vim,
            "an unrecognized preset name warns and falls back to vim rather than crashing"
        );
    }

    #[test]
    fn auto_install_defaults_to_true() {
        let repo = fixture_repo();
        let config = load_merged(&repo);
        assert!(config.auto_install);
    }

    #[test]
    fn auto_install_can_be_disabled_explicitly() {
        let repo = fixture_repo();
        write_repo_config(&repo, "[lsp]\nauto_install = false\n");
        let config = load_merged(&repo);
        assert!(!config.auto_install);
        // Disabling auto-install doesn't reset `[lsp.servers]` overrides
        // set elsewhere — same field-level merge guarantee every other
        // section gets.
        assert!(config.lsp_servers.is_empty());
    }

    #[test]
    fn lsp_server_override_parses_command_and_args() {
        let repo = fixture_repo();
        write_repo_config(
            &repo,
            "[lsp.servers.rust]\ncommand = \"/opt/bin/rust-analyzer\"\nargs = [\"--foo\"]\n",
        );
        let config = load_merged(&repo);
        let rust = config.lsp_servers.get("rust").unwrap();
        assert_eq!(rust.command, "/opt/bin/rust-analyzer");
        assert_eq!(rust.args, vec!["--foo".to_owned()]);
    }
}
