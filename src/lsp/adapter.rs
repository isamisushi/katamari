//! Which language server to launch for a given file, and where its
//! workspace root is. [`crate::lsp::manager::LspManager`] is the only
//! caller — everything it needs to know about *how* to start a server for a
//! language lives here, behind [`resolve_server`], so adding a language
//! means adding one more match arm, not touching the manager's
//! spawn/queue/state-machine logic.

use crate::config::ServerOverride;
use crate::lsp::install;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A language `LspManager` knows how to find a server for.
/// [`Language::detect`] returning `None` for every other extension is what
/// makes "no LSP support for this file type" a normal, silent outcome
/// rather than a special case each call site has to guard against
/// separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Kotlin,
    Java,
}

impl Language {
    pub fn detect(path: &Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Language::Rust),
            Some("ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts") => {
                Some(Language::TypeScript)
            }
            Some("py" | "pyi") => Some(Language::Python),
            Some("go") => Some(Language::Go),
            Some("kt" | "kts") => Some(Language::Kotlin),
            Some("java") => Some(Language::Java),
            _ => None,
        }
    }

    /// The `languageId` sent in `textDocument/didOpen` — part of the LSP
    /// spec's vocabulary, not this codebase's; servers key syntax/feature
    /// behavior off it. `TypeScript` covers four extensions
    /// (ts/tsx/js/jsx and their module-suffix variants), each of which
    /// needs its own `languageId` for typescript-language-server to parse
    /// it with the right grammar — [`lsp_language_id`] does that
    /// extension-aware lookup; this method exists for [`resolve_server`]
    /// and tests that only need "some valid id for this language family",
    /// where any one of the four is equally fine.
    pub fn lsp_id(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::TypeScript => "typescript",
            Language::Python => "python",
            Language::Go => "go",
            Language::Kotlin => "kotlin",
            Language::Java => "java",
        }
    }
}

/// The precise `languageId` LSP expects for `path`, given it's already been
/// routed to `language` by [`Language::detect`]. Only [`Language::TypeScript`]
/// actually varies by extension — a `.jsx` file must announce itself as
/// `javascriptreact`, not `typescript`, or typescript-language-server
/// applies the wrong grammar and JSX tags stop parsing.
pub fn lsp_language_id(language: Language, path: &Path) -> &'static str {
    if language != Language::TypeScript {
        return language.lsp_id();
    }
    match path.extension().and_then(|e| e.to_str()) {
        Some("tsx") => "typescriptreact",
        Some("jsx") => "javascriptreact",
        Some("js" | "mjs" | "cjs") => "javascript",
        _ => "typescript",
    }
}

/// The server-identity type [`crate::lsp::manager::LspManager`] and the UI
/// key servers by — a strict superset of [`Language`], which stays
/// `Copy`/closed and keeps driving every built-in dispatch table
/// (`command_for`, `install_hint`, `root_markers`, ...) unchanged. A
/// `Custom(id)` is a `[lsp.servers.<id>]` config entry whose `<id>` isn't
/// one of the six built-in [`Language::lsp_id`]s and which claims at least
/// one file extension via [`ServerOverride::extensions`] — see
/// [`custom_extension_map`]. Not `Copy` (the `Custom` variant owns a
/// `String`), unlike `Language` — every call site that used to get a free
/// copy of a `Language` key now needs an explicit `.clone()` where it
/// crosses a loop iteration or an already-borrowed `ServerKey`; see
/// [`crate::lsp::manager`]'s doc comments at each such site for why.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LangKey {
    Builtin(Language),
    Custom(String),
}

impl std::fmt::Display for LangKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LangKey::Builtin(language) => write!(f, "{}", language.lsp_id()),
            LangKey::Custom(id) => write!(f, "{id}"),
        }
    }
}

impl LangKey {
    /// Routes `path` to a server identity: [`Language::detect`] first,
    /// unconditionally — a custom id can never steal an extension a
    /// built-in language already owns (see [`custom_extension_map`]'s
    /// docs on why that collision is dropped, with a warning, at map-build
    /// time rather than resolved here per-file), so there is nothing left
    /// to check once a built-in hit is found. Only when nothing built-in
    /// claims `path`'s extension does `custom_extensions` (built once per
    /// [`crate::lsp::manager::LspManager`] — see [`custom_extension_map`])
    /// get consulted.
    pub fn detect(path: &Path, custom_extensions: &HashMap<String, String>) -> Option<LangKey> {
        if let Some(language) = Language::detect(path) {
            return Some(LangKey::Builtin(language));
        }
        let ext = path.extension().and_then(|e| e.to_str())?;
        custom_extensions.get(ext).cloned().map(LangKey::Custom)
    }
}

/// The `languageId` [`crate::lsp::manager::LspManager::dispatch`] announces
/// in `didOpen` for either kind of [`LangKey`]: a built-in key delegates
/// straight to [`lsp_language_id`] (unchanged); a custom key uses its
/// entry's [`ServerOverride::language_id`] override if it set one, else
/// falls back to the id itself — the common case, since most servers are
/// happy being told their own config key back (`"ruby"` for a
/// `[lsp.servers.ruby]` entry running a Ruby server). Returns an owned
/// `String` rather than `lsp_language_id`'s `&'static str`, since a custom
/// id's value only ever lives as long as `overrides` does.
pub fn lsp_language_id_for(
    key: &LangKey,
    path: &Path,
    overrides: &HashMap<String, ServerOverride>,
) -> String {
    match key {
        LangKey::Builtin(language) => lsp_language_id(*language, path).to_owned(),
        LangKey::Custom(id) => overrides
            .get(id)
            .and_then(|over| over.language_id.clone())
            .unwrap_or_else(|| id.clone()),
    }
}

/// Derives `{extension -> custom id}` from every `overrides` entry that
/// claims at least one extension — this is the whole config surface for a
/// custom server: no separate `[lsp.custom]` table, just an ordinary
/// `[lsp.servers.<id>]` entry with a non-empty `extensions` list (see
/// [`ServerOverride::extensions`]'s docs). Built once per
/// [`crate::lsp::manager::LspManager`] (in its constructor) rather than
/// per-file, since `overrides` itself never changes for the life of a
/// session.
///
/// An id equal to one of the six built-in [`Language::lsp_id`] strings is
/// never eligible to make a custom claim at all — `<id>` is what decides
/// override-vs-custom (see [`ServerOverride`]'s docs), so a built-in-named
/// entry's `extensions` (a plausible copy-paste-and-adapt mistake, e.g.
/// reusing `[lsp.servers.rust]` while experimenting with an unrelated
/// filetype) is ignored outright, with a stderr warning, rather than
/// treated as a second, `Display`-indistinguishable shadow server
/// alongside the real built-in one — see [`is_builtin_language_id`].
///
/// Each remaining extension is normalized before anything else — trimmed
/// of surrounding whitespace, then stripped of at most one leading `.` — so
/// `extensions = [".rb"]` and `extensions = ["rb"]` claim the identical
/// extension; [`Path::extension()`] never includes the leading dot, so
/// without this an entry written the VSCode-`files.associations` way would
/// silently never route to anything, with no warning anywhere to explain
/// why.
///
/// Two collisions are then possible, both resolved the same deterministic
/// way — process one id's claims, in a fixed (lexicographic) order, and
/// drop (with a stderr warning) any claim that loses to a claim already
/// recorded:
/// - **A custom id claims an extension a built-in [`Language`] already
///   owns** (checked via [`Language::detect`] on a throwaway path with that
///   extension, so this can never silently drift out of sync with
///   `Language::detect`'s own match arms) — the built-in always wins; this
///   mirrors [`LangKey::detect`]'s own built-in-first precedence, just
///   caught earlier, at map-build time, so the warning fires once instead
///   of once per file.
/// - **Two custom ids claim the same extension** — processing ids in
///   ascending lexicographic order and never overwriting an
///   already-claimed extension means the first (lexicographically
///   smallest) id to claim it always wins, deterministically, regardless of
///   `overrides`' `HashMap` iteration order (which is randomized per
///   process and would otherwise make the winner flip from run to run).
pub fn custom_extension_map(
    overrides: &HashMap<String, ServerOverride>,
) -> HashMap<String, String> {
    let mut ids: Vec<&String> = overrides.keys().collect();
    ids.sort();
    let mut map: HashMap<String, String> = HashMap::new();
    for id in ids {
        let over = &overrides[id];
        if is_builtin_language_id(id) {
            if !over.extensions.is_empty() {
                eprintln!(
                    "katamari: warning: `[lsp.servers.{id}]`'s `extensions` field is ignored — \
                     `{id}` overrides the built-in {id} server, it doesn't define a custom one"
                );
            }
            continue;
        }
        for raw_ext in &over.extensions {
            let ext = normalize_extension(raw_ext);
            if builtin_owns_extension(ext) {
                eprintln!(
                    "katamari: warning: custom lsp server `{id}` claims extension `.{ext}`, \
                     which a built-in language server already handles — ignored"
                );
                continue;
            }
            if let Some(existing) = map.get(ext) {
                eprintln!(
                    "katamari: warning: custom lsp server `{id}` also claims extension \
                     `.{ext}`, already claimed by `{existing}` — `{id}`'s claim is ignored"
                );
                continue;
            }
            map.insert(ext.to_owned(), id.clone());
        }
    }
    map
}

/// Whether `id` names one of the six built-in languages' own
/// [`Language::lsp_id`] — derived from `Language`'s own variants rather than
/// a second, hand-maintained string list, the same anti-drift principle
/// [`builtin_owns_extension`] already applies to the built-in extension
/// set. [`custom_extension_map`] uses this to keep a built-in-named
/// `[lsp.servers.<id>]` entry from ever defining a custom filetype claim;
/// `ktmr lsp doctor`'s custom-server table (`main.rs`'s
/// `print_custom_server_doctor`) reuses it too, to annotate exactly such an
/// entry instead of reporting its (inert) `extensions` at face value.
pub(crate) fn is_builtin_language_id(id: &str) -> bool {
    const ALL: [Language; 6] = [
        Language::Rust,
        Language::TypeScript,
        Language::Python,
        Language::Go,
        Language::Kotlin,
        Language::Java,
    ];
    ALL.iter().any(|language| language.lsp_id() == id)
}

/// Normalizes one raw `extensions` entry the same way [`custom_extension_map`]
/// does before ever using it as a map key: trims surrounding whitespace,
/// then strips at most one leading `.` — [`Path::extension()`] never
/// includes the dot, so `".rb"` and `"rb"` must resolve to the identical
/// extension or a leading-dot entry silently never routes to anything (see
/// [`custom_extension_map`]'s docs). `pub(crate)` so `ktmr lsp doctor`'s
/// custom-server table (`main.rs`) can display and re-check an entry's
/// extensions the exact same way this module actually routes them, instead
/// of re-deriving (and risking drifting from) this same trim-and-strip rule
/// a second time.
pub(crate) fn normalize_extension(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed.strip_prefix('.').unwrap_or(trimmed)
}

/// Whether `ext` (no leading dot) is one [`Language::detect`] already
/// routes to a built-in language — probed via a throwaway `x.<ext>` path
/// rather than a second, hand-maintained extension list, so this can never
/// quietly drift out of sync with `Language::detect`'s own match arms.
fn builtin_owns_extension(ext: &str) -> bool {
    Language::detect(&PathBuf::from(format!("x.{ext}"))).is_some()
}

/// Builds the `Command` for a *custom* (non-built-in) [`LangKey::Custom`]
/// server straight from its `[lsp.servers.<id>]` entry: no lookup tiers (no
/// PATH/mise/project-local search the way a built-in language gets — the
/// user's `command` *is* the resolution), and never installable (a custom
/// server is, by definition, something this module has no install recipe
/// for). An empty `command` is the one way this can fail — everything else
/// about the entry (missing `args`, no `initialization_options`) is
/// perfectly valid and just resolves to defaults.
pub fn resolve_custom_server(
    id: &str,
    over: &ServerOverride,
) -> Result<ResolvedServer, Unavailable> {
    if over.command.trim().is_empty() {
        return Err(unavailable(
            id,
            "custom server has no command configured — set `command` under \
             `[lsp.servers.<id>]`",
            false,
        ));
    }
    let mut command = Command::new(&over.command);
    command.args(&over.args);
    Ok(ResolvedServer {
        command,
        initialization_options: over.initialization_options.as_ref().map(toml_value_to_json),
    })
}

/// Total (never-fails) conversion from a parsed TOML value to the JSON
/// shape `initializationOptions` (and everything else this module hands to
/// `serde_json`) needs. The two type systems agree on every `toml::Value`
/// variant except two:
/// - `Datetime` has no JSON equivalent at all — stringified via its own
///   `Display` (RFC 3339), the same lossless bridge every other
///   TOML-to-JSON converter uses.
/// - `Float` can be NaN/infinite, which JSON's number grammar can't
///   represent — `serde_json::Number::from_f64` already returns `None` for
///   those, so they become `Null` rather than this function panicking or
///   returning a `Result` its one caller (a config-loading path with no
///   good way to fail loudly this late) would just have to unwrap anyway.
pub fn toml_value_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(dt) => serde_json::Value::String(dt.to_string()),
        toml::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(toml_value_to_json).collect())
        }
        toml::Value::Table(table) => serde_json::Value::Object(
            table
                .iter()
                .map(|(k, v)| (k.clone(), toml_value_to_json(v)))
                .collect(),
        ),
    }
}

/// Why no server could be started, in a form suitable for a status-bar
/// message — this is what a user sees when a language feature doesn't work,
/// so it says what to do about it, not just what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub reason: String,
    /// Whether [`crate::lsp::install::ensure`] could plausibly turn this
    /// into a working server — checked here (a cheap subprocess probe for
    /// gopls's go-toolchain prerequisite and jdtls's JDK prerequisite;
    /// unconditionally `true` for the other four languages, see
    /// [`diagnose_language`]'s docs) rather than by actually attempting an
    /// install, so `resolve_server` stays network-free.
    /// [`crate::lsp::manager::LspManager::spawn_server`] reads
    /// this to decide whether to call `install::ensure` at all when
    /// `[lsp] auto_install` is on.
    pub installable: bool,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// [`resolve_server`]'s success case: the `Command` to spawn, plus whatever
/// extra `initialize` `initializationOptions` that particular resolution
/// needs beyond katamari's generic handshake (see
/// [`crate::lsp::client::Client::start`]). `None` for every resolution
/// except a katamari-managed typescript-language-server, which needs to be
/// told exactly where its peer-installed `typescript` lives — the server's
/// own default resolution walks up from the *edited* workspace, which
/// katamari's managed install directory was never part of, so without this
/// it fails to initialize entirely for a project with no `typescript`
/// devDependency of its own (found by hand during M8b's manual E2E
/// verification — see [`crate::lsp::install::managed_tsserver_path`]'s
/// docs for the full story).
pub struct ResolvedServer {
    pub command: Command,
    pub initialization_options: Option<serde_json::Value>,
}

/// Builds the command to launch `language`'s server. Never runs it, and
/// never touches the network — spawning is
/// [`crate::lsp::transport::Transport::spawn`]'s job, and installing a
/// missing server is [`crate::lsp::install::ensure`]'s, called only from
/// [`crate::lsp::manager::LspManager::spawn_server`] once this function has
/// already said `Unavailable { installable: true, .. }`. That split is what
/// keeps this whole module testable as pure path resolution even though it
/// now has five lookup tiers instead of two. `overrides` (config's
/// `[lsp.servers.<lang>]`, keyed by [`Language::lsp_id`]) takes priority
/// over every built-in lookup below when it has an entry for `language`,
/// exactly as a user's explicit config should — a pinned server version, a
/// wrapper script, or a server this module has no built-in support for at
/// all.
pub fn resolve_server(
    language: Language,
    workspace_root: &Path,
    overrides: &HashMap<String, ServerOverride>,
) -> Result<ResolvedServer, Unavailable> {
    if let Some(over) = overrides.get(language.lsp_id()) {
        let mut command = Command::new(&over.command);
        command.args(&over.args);
        return Ok(ResolvedServer {
            command,
            initialization_options: over.initialization_options.as_ref().map(toml_value_to_json),
        });
    }
    let (hit, installable) = diagnose_language(language, workspace_root);
    match hit {
        Some(hit) => command_for(language, workspace_root, hit),
        None => Err(unavailable(
            language.lsp_id(),
            &install_hint(language, installable),
            installable,
        )),
    }
}

/// `ktmr lsp doctor`'s per-language finding: either where `language`'s
/// server resolves from today (as [`resolve_server`] would use it), or —
/// when nowhere — whether auto-install could handle it. Shares
/// [`diagnose_language`] with `resolve_server` so the two can never
/// disagree about what's found; the only thing this adds is *which* tier
/// found it, information `resolve_server`'s `Result<Command, _>` has no
/// reason to carry for its own caller.
pub fn diagnose(
    language: Language,
    workspace_root: &Path,
    overrides: &HashMap<String, ServerOverride>,
) -> Diagnosis {
    if let Some(over) = overrides.get(language.lsp_id()) {
        return Diagnosis {
            language,
            found: Some((ResolvedFrom::ConfigOverride, PathBuf::from(&over.command))),
            installable_if_missing: false,
        };
    }
    let (hit, installable) = diagnose_language(language, workspace_root);
    Diagnosis {
        language,
        found: hit.map(|hit| (hit.source(), hit.into_path())),
        installable_if_missing: installable,
    }
}

#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub language: Language,
    /// `Some((tier, path))` when a server was found; the tier it was found
    /// through and the resolved path.
    pub found: Option<(ResolvedFrom, PathBuf)>,
    /// Only meaningful when `found` is `None`: whether
    /// [`crate::lsp::install::ensure`] could plausibly install this
    /// language's server.
    pub installable_if_missing: bool,
}

/// Which tier of the shared lookup order (see [`lookup_in_order`]) a hit
/// came from — the detail [`Diagnosis`] exposes for `ktmr lsp doctor` that a
/// plain `Result<Command, Unavailable>` has no need to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolvedFrom {
    ConfigOverride,
    ProjectLocal,
    Path,
    ToolchainWhich,
    Mise,
    KatamariManaged,
}

/// The six-way dispatch [`resolve_server`] and [`diagnose`] both build on:
/// which lookup tier (if any) finds `language`'s binary, and — only
/// meaningful when none did — whether it's installable. Go and Java are the
/// two languages where that second question needs an actual environment
/// check rather than a flat `true`: Go needs a go toolchain reachable to
/// run `go install` against (see [`go_toolchain_available`]), Java needs a
/// usable JDK reachable to launch jdtls with at all (see [`probe_java`]) —
/// katamari won't install a JVM, so a found-but-too-old or altogether
/// missing JDK makes jdtls just as uninstallable as an absent go toolchain
/// makes gopls. The other four are always installable when missing, since
/// rust-analyzer and kotlin-lsp both download as self-contained archives
/// (kotlin-lsp bundles its own JetBrains Runtime, so unlike Go it never
/// needs an external JVM the way `go install` needs an external Go
/// toolchain — see [`install::install_kotlin_lsp`]'s docs) and the two
/// npm-based servers bootstrap their own Node.js runtime when npm can't be
/// found anywhere.
fn diagnose_language(language: Language, workspace_root: &Path) -> (Option<LookupHit>, bool) {
    match language {
        Language::Rust => (lookup_rust_analyzer(), true),
        Language::TypeScript => (lookup_typescript_language_server(workspace_root), true),
        Language::Python => (lookup_pyright(workspace_root), true),
        Language::Go => {
            let hit = lookup_gopls();
            let installable = hit.is_some() || go_toolchain_available();
            (hit, installable)
        }
        Language::Kotlin => (lookup_kotlin_lsp(), true),
        // A found jdtls with no usable JDK must NOT be reported as
        // available — it would die at spawn (`jdtls requires at least Java
        // 21`, then exit) and a naive caller would misread the resulting
        // silence as a 30s initialize timeout rather than the actionable
        // "install a JDK" message `install_hint` gives instead. So the JDK
        // is gated on *before* even bothering to look for jdtls itself.
        Language::Java => match probe_java() {
            JavaProbe::Ok(_) => (lookup_jdtls(), true),
            JavaProbe::TooOld { .. } | JavaProbe::NotFound => (None, false),
        },
    }
}

/// `pub(crate)` (not just [`resolve_server`]'s own use, below) so
/// [`crate::doctor`]'s lsp-resolution section can build the same
/// not-found/install hint text `ktmr lsp doctor` and a live session's
/// status bar already show — see [`resolve_server`]'s docs on why sharing
/// this instead of re-deriving it matters: two independently-written hints
/// could drift apart, and a diagnostic that disagrees with the thing it's
/// diagnosing is worse than useless.
pub(crate) fn install_hint(language: Language, installable: bool) -> String {
    match language {
        Language::Rust => "rust-analyzer not found on PATH, via `rustup which`, `mise which`, or \
            katamari's managed install — install it with `rustup component add rust-analyzer`, \
            or let katamari auto-install it (`ktmr lsp install rust`)"
            .to_owned(),
        Language::TypeScript => "typescript-language-server not found on PATH, via `mise which`, \
            or katamari's managed install — install it with \
            `npm i -g typescript-language-server typescript`, or let katamari auto-install it \
            (`ktmr lsp install typescript`)"
            .to_owned(),
        Language::Python => "pyright-langserver not found on PATH, via `mise which`, or \
            katamari's managed install — install it with `npm i -g pyright`, or let katamari \
            auto-install it (`ktmr lsp install python`)"
            .to_owned(),
        Language::Go if installable => "gopls not found on PATH, via `mise which`, or katamari's \
            managed install — install it with `go install golang.org/x/tools/gopls@latest`, or \
            let katamari auto-install it (`ktmr lsp install go`)"
            .to_owned(),
        Language::Go => "gopls not found, and no go toolchain is reachable to auto-install it \
            with — install Go first (https://go.dev/dl/), then \
            `go install golang.org/x/tools/gopls@latest`"
            .to_owned(),
        Language::Kotlin => "kotlin-lsp not found on PATH, via `mise which`, or katamari's \
            managed install — install it from https://github.com/Kotlin/kotlin-lsp, or let \
            katamari auto-install it (`ktmr lsp install kotlin`)"
            .to_owned(),
        // Three cases, not two like Go's — Java's missing-prerequisite
        // question has an extra branch (found-but-too-old) that Go doesn't,
        // since a go toolchain has no version floor the way jdtls's JDK
        // does. `installable` (the caller-computed bool) collapses
        // `TooOld`/`NotFound` to the same `false`, so the distinction is
        // re-derived here directly from a fresh probe rather than threaded
        // through as a parameter — see `diagnose_language`'s Java arm for
        // why this reprobe is cheap enough to not matter (only reached on
        // the already-slow "nothing found" path).
        Language::Java => java_install_hint(),
    }
}

/// The three-way Java-specific hint [`install_hint`] delegates to: unlike
/// every other language here, "not found" isn't the only way Java can be
/// unavailable — a JDK that exists but is too old is a fundamentally
/// different, more actionable message (name the path and version found,
/// rather than "install something") than either "no JDK at all" or "JDK's
/// fine, jdtls itself is what's missing".
fn java_install_hint() -> String {
    match probe_java() {
        JavaProbe::NotFound => format!(
            "jdtls needs a JDK {JDTLS_MIN_JAVA_MAJOR}+ via `$JAVA_HOME`, `PATH`, or `mise` — \
             install one first (e.g. `mise use java@21`); katamari won't install a JVM"
        ),
        JavaProbe::TooOld { path, major } => format!(
            "found a JDK at {} (Java {major}), but jdtls requires Java {JDTLS_MIN_JAVA_MAJOR}+ — \
             install a newer JDK (e.g. `mise use java@21`); katamari won't install a JVM",
            path.display()
        ),
        JavaProbe::Ok(_) => "jdtls not found on PATH, via `mise which`, or katamari's managed \
            install — install it with `ktmr lsp install java`, or e.g. `brew install jdtls`"
            .to_owned(),
    }
}

/// The JDK half of `ktmr lsp doctor`'s Java row: unlike [`diagnose`], which
/// only ever reports on *jdtls itself* (and — since `diagnose_language`
/// gates jdtls's lookup on a usable JDK existing at all, see that
/// function's docs — collapses any JDK problem straight to "not found"
/// with no path or version), this names the actual JDK
/// [`probe_java`] found: its path and major version when it's usable, the
/// same when it's found but too old (so a user can tell "no JDK at all"
/// apart from "found one, but it needs upgrading" — the same distinction
/// [`java_install_hint`] exists to make, just phrased for a doctor note
/// instead of an install hint), or that none was found. Re-probes rather
/// than threading `diagnose`'s own result through, mirroring
/// [`java_install_hint`]'s reprobe — cheap enough not to matter on the
/// doctor's already print-only, offline path.
pub fn java_jdk_note() -> String {
    java_jdk_status().1
}

/// `(a usable JDK was found, [`java_jdk_note`]'s note text)` — the same
/// [`probe_java`] outcome, computed once so a caller that needs to *tag* the
/// note (`doctor.rs`'s Java sub-check) can never disagree with what the note
/// itself says. Before this existed, `doctor.rs` hardcoded that row to
/// `Check::ok` unconditionally, so a report could show `ok  java: jdk: not
/// found` — a green tag directly contradicting its own text — for exactly
/// the JDK-missing/too-old cases this dimension exists to catch; deriving
/// both halves from one probe closes that gap structurally rather than
/// relying on a caller to remember to match them up by hand.
pub(crate) fn java_jdk_status() -> (bool, String) {
    match probe_java() {
        JavaProbe::Ok(java) => (
            true,
            format!("{} (Java {})", java.path.display(), java.major),
        ),
        JavaProbe::TooOld { path, major } => (
            false,
            format!(
                "{} (Java {major}, jdtls needs {JDTLS_MIN_JAVA_MAJOR}+)",
                path.display()
            ),
        ),
        JavaProbe::NotFound => (false, "not found".to_owned()),
    }
}

fn command_for(
    language: Language,
    workspace_root: &Path,
    hit: LookupHit,
) -> Result<ResolvedServer, Unavailable> {
    let from_katamari_managed = matches!(hit, LookupHit::KatamariManaged(_));
    let path = hit.into_path();
    match language {
        Language::TypeScript => {
            // A project-local or `PATH` server is left to its own default
            // `typescript` resolution (more likely to already agree with
            // that project's own devDependency); katamari's own managed
            // install isn't reachable that way, so it needs pointing at
            // its peer-installed `typescript` explicitly, via
            // `initializationOptions.tsserver.path` — see
            // `install::managed_tsserver_path`'s and `ResolvedServer`'s
            // docs for why this can't just be a CLI flag.
            let initialization_options = from_katamari_managed
                .then(install::managed_tsserver_path)
                .filter(|p| p.is_file())
                .map(|tsserver| {
                    serde_json::json!({ "tsserver": { "path": tsserver.display().to_string() } })
                });
            let mut command = ts_language_server_command(path);
            if from_katamari_managed {
                prepend_bootstrapped_node_path(&mut command);
            }
            Ok(ResolvedServer {
                command,
                initialization_options,
            })
        }
        Language::Python => {
            let mut command = pyright_command(path);
            if from_katamari_managed {
                prepend_bootstrapped_node_path(&mut command);
            }
            Ok(ResolvedServer {
                command,
                initialization_options: None,
            })
        }
        Language::Kotlin => Ok(ResolvedServer {
            command: kotlin_lsp_command(path),
            initialization_options: None,
        }),
        Language::Rust | Language::Go => Ok(ResolvedServer {
            command: Command::new(path),
            initialization_options: None,
        }),
        Language::Java => java_command_for(workspace_root, from_katamari_managed, path),
    }
}

/// Builds the Java arm's `Command` — the only language whose launch shape
/// depends on *which* lookup tier hit (see the two-branch split below), and
/// the only one that can still fail here even after `diagnose_language`
/// already said "usable" (defense in depth: java could have vanished from
/// `PATH`/`JAVA_HOME`/mise between that check and this call, however
/// unlikely in practice).
fn java_command_for(
    workspace_root: &Path,
    from_katamari_managed: bool,
    path: PathBuf,
) -> Result<ResolvedServer, Unavailable> {
    let data_dir = jdtls_data_dir(workspace_root);
    let _ = std::fs::create_dir_all(&data_dir);

    let mut command = if from_katamari_managed {
        // `path` here is `install::binary_path`'s pinned launcher-jar path,
        // i.e. `<install_dir>/plugins/<JDTLS_LAUNCHER_JAR>` — two `parent()`
        // calls back out to `install_dir`, the root jdtls needs for its
        // `-Dosgi.sharedConfiguration.area` and the `-jar` flag alike (see
        // `jdtls_managed_launch_args`).
        let install_dir = path.parent().and_then(Path::parent).ok_or_else(|| {
            unavailable(
                Language::Java.lsp_id(),
                "katamari's managed jdtls install is missing its plugins directory — try `ktmr \
                 lsp install java` again",
                true,
            )
        })?;
        let java = resolve_java()
            .ok_or_else(|| unavailable(Language::Java.lsp_id(), &java_install_hint(), false))?;
        let args = jdtls_managed_launch_args(&java.path, java.major, install_dir, &data_dir)
            .map_err(|reason| unavailable(Language::Java.lsp_id(), &reason, true))?;
        let mut args = args.into_iter();
        let program = args
            .next()
            .expect("jdtls_managed_launch_args always emits java_path first");
        let mut command = Command::new(program);
        command.args(args);
        command
    } else {
        // The user's own `jdtls` wrapper script (found on `PATH` or via
        // `mise`) resolves its own java/config already — only `-data` needs
        // overriding, since the wrapper's built-in default keys off the
        // workspace's basename alone and would collide across two
        // same-named directories on different paths.
        let mut command = Command::new(path);
        command.arg("-data").arg(&data_dir);
        command
    };
    // An inherited `CLIENT_PORT` silently flips jdtls into socket mode
    // instead of stdio — breaking this client, which only ever speaks
    // stdio — so it (and its companion `CLIENT_HOST`) must never survive
    // into the child's environment, on either launch shape.
    command.env_remove("CLIENT_PORT").env_remove("CLIENT_HOST");
    Ok(ResolvedServer {
        command,
        initialization_options: None,
    })
}

/// The exact `java` invocation `bin/jdtls.py` itself uses inside the real
/// jdtls tarball (verified by hand against jdtls 1.60.0 — see this crate's
/// Java-support design notes for the full ground-truth launch line), for a
/// katamari-managed install rooted at `install_dir` with a per-workspace
/// index directory of `data_dir`. Returns a full argv (`argv[0]` is
/// `java_path` itself — the POSIX exec convention, and the reason callers
/// split it back into `Command::new(argv[0]).args(&argv[1..])`) rather than
/// a `Command`, so the exact flag sequence is a plain `Vec` a unit test can
/// assert on without spawning anything. `major` decides whether the two
/// XML-entity-limit flags are prepended: Java 24 hardened its XML parser's
/// default entity-size limits in a way that trips over jdtls's own
/// generated project metadata on larger workspaces, so upstream only raises
/// those limits for Java 24+ — sending them to an older JDK that doesn't
/// recognize the flag would be the actual error, not a no-op, which is why
/// this is conditional rather than unconditional.
fn jdtls_managed_launch_args(
    java_path: &Path,
    major: u32,
    install_dir: &Path,
    data_dir: &Path,
) -> Result<Vec<OsString>, String> {
    let config_dir = install_dir.join(jdtls_config_dir_name(std::env::consts::OS)?);
    let launcher_jar = install_dir
        .join("plugins")
        .join(install::JDTLS_LAUNCHER_JAR);

    let mut args: Vec<OsString> = vec![java_path.into()];
    if major >= JDTLS_XML_ENTITY_LIMIT_MIN_JAVA_MAJOR {
        args.push(OsString::from("-Djdk.xml.maxGeneralEntitySizeLimit=0"));
        args.push(OsString::from("-Djdk.xml.totalEntitySizeLimit=0"));
    }
    args.push(OsString::from(
        "-Declipse.application=org.eclipse.jdt.ls.core.id1",
    ));
    args.push(OsString::from("-Dosgi.bundles.defaultStartLevel=4"));
    args.push(OsString::from(
        "-Declipse.product=org.eclipse.jdt.ls.core.product",
    ));
    args.push(OsString::from("-Dosgi.checkConfiguration=true"));
    // Built with `OsString::push` rather than `format!`/`Path::display()` —
    // `display()` is lossy (substitutes U+FFFD for invalid UTF-8), which on
    // a non-UTF8 `config_dir` would launch jdtls with a `-D` flag pointing
    // at a path that doesn't exist, distinct from the real on-disk
    // directory — the same lossless approach the `-jar`/`-data` args below
    // already use.
    let mut shared_config_area = OsString::from("-Dosgi.sharedConfiguration.area=");
    shared_config_area.push(config_dir.as_os_str());
    args.push(shared_config_area);
    args.push(OsString::from(
        "-Dosgi.sharedConfiguration.area.readOnly=true",
    ));
    args.push(OsString::from("-Dosgi.configuration.cascaded=true"));
    args.push(OsString::from("-Xms1G"));
    args.push(OsString::from("--add-modules=ALL-SYSTEM"));
    args.push(OsString::from("--add-opens"));
    args.push(OsString::from("java.base/java.util=ALL-UNNAMED"));
    args.push(OsString::from("--add-opens"));
    args.push(OsString::from("java.base/java.lang=ALL-UNNAMED"));
    args.push(OsString::from("-jar"));
    args.push(launcher_jar.into_os_string());
    args.push(OsString::from("-data"));
    args.push(data_dir.into());
    Ok(args)
}

/// The Java major version at which upstream jdtls.py starts prepending the
/// two `-Djdk.xml.*EntitySizeLimit=0` flags to its own launch line — see
/// [`jdtls_managed_launch_args`]'s docs.
const JDTLS_XML_ENTITY_LIMIT_MIN_JAVA_MAJOR: u32 = 24;

/// Maps a target OS to jdtls's own OS-keyed shared-configuration directory
/// name — verified against the real jdt-language-server tarball: only
/// `config_linux`/`config_mac`/`config_win` are ever selected, even though
/// the tarball also ships `config_linux_arm` and a handful of `config_ss_*`
/// variants for specialized targets — upstream's own `bin/jdtls.py` wrapper
/// never selects those either, so this mirrors that rather than trying to
/// be smarter than upstream. Shared between [`jdtls_managed_launch_args`]
/// (which needs the directory to point `-Dosgi.sharedConfiguration.area`
/// at) and [`install::install_jdtls`] (which needs it to sanity-check a
/// fresh extraction actually has the directory this platform will need).
pub(crate) fn jdtls_config_dir_name(os: &str) -> Result<&'static str, String> {
    match os {
        "linux" => Ok("config_linux"),
        "macos" => Ok("config_mac"),
        "windows" => Ok("config_win"),
        other => Err(format!(
            "katamari's managed jdtls has no shared-configuration directory for {other}"
        )),
    }
}

/// The minimum JDK major version jdtls requires — anything older fails fast
/// with "jdtls requires at least Java 21" on stderr rather than actually
/// starting (see [`crate::lsp::client`]'s stderr-on-failure wiring, which is
/// what turns that message into something a user actually sees instead of a
/// bare timeout).
const JDTLS_MIN_JAVA_MAJOR: u32 = 21;

/// A JDK found on this machine, resolved enough to launch jdtls with: its
/// `java` executable's absolute path (jdtls.py itself never invokes a bare
/// `"java"`, and neither does [`java_command_for`]) and the major version
/// parsed from `java -version`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedJava {
    path: PathBuf,
    major: u32,
}

/// The three states a JDK probe can land in — kept distinct rather than
/// collapsing straight to `Option<ResolvedJava>` because [`java_install_hint`]
/// needs to tell "no JDK anywhere" apart from "found one, but it's too old"
/// and name the offending path/version in the second case; a found-but-wrong
/// JDK is a meaningfully different, more actionable message than nothing at
/// all.
#[derive(Debug, Clone, PartialEq, Eq)]
enum JavaProbe {
    Ok(ResolvedJava),
    TooOld { path: PathBuf, major: u32 },
    NotFound,
}

/// Finds a usable JDK for jdtls: `$JAVA_HOME/bin/java` first (jdtls.py's own
/// precedence), then `java` on `PATH`, then `mise which java`. Every
/// candidate that exists gets version-probed in turn — a too-old one
/// doesn't stop the search early (a `JAVA_HOME` left pointing at Java 8 for
/// some unrelated project shouldn't hide a perfectly good Java 21 on
/// `PATH`), but if every candidate found is too old, the *first* one found
/// is what [`java_install_hint`] names, since that's the one most likely to
/// be the JDK a user actually set up on purpose.
fn probe_java() -> JavaProbe {
    let candidates = [
        java_home_candidate(std::env::var_os("JAVA_HOME")),
        which_on_path("java"),
        mise_which("java"),
    ];
    let mut too_old = None;
    for candidate in candidates.into_iter().flatten() {
        let Some(major) = java_major_version(&candidate) else {
            continue;
        };
        if major >= JDTLS_MIN_JAVA_MAJOR {
            return JavaProbe::Ok(ResolvedJava {
                path: candidate,
                major,
            });
        }
        if too_old.is_none() {
            too_old = Some((candidate, major));
        }
    }
    match too_old {
        Some((path, major)) => JavaProbe::TooOld { path, major },
        None => JavaProbe::NotFound,
    }
}

/// The success-only view of [`probe_java`], for [`java_command_for`]'s cheap
/// re-probe — it only cares whether a usable JDK still exists at spawn
/// time, not why one doesn't (that detail is [`java_install_hint`]'s job,
/// reached through `diagnose_language`'s own, earlier probe).
fn resolve_java() -> Option<ResolvedJava> {
    match probe_java() {
        JavaProbe::Ok(java) => Some(java),
        JavaProbe::TooOld { .. } | JavaProbe::NotFound => None,
    }
}

/// `$JAVA_HOME/bin/java`, if `$JAVA_HOME` is set and that file actually
/// exists — jdtls.py's own first candidate before falling back to `PATH`.
fn java_home_candidate(java_home: Option<std::ffi::OsString>) -> Option<PathBuf> {
    let candidate = PathBuf::from(java_home?).join("bin").join("java");
    candidate.is_file().then_some(candidate)
}

/// Runs `<path> -version` and parses its output for a major version — see
/// [`parse_java_major_version`] for the actual parsing, kept pure and
/// separate from this subprocess call so it's testable against fixed
/// strings pulled from real JDK output without needing a real `java`
/// binary in CI.
fn java_major_version(path: &Path) -> Option<u32> {
    let output = Command::new(path).arg("-version").output().ok()?;
    // `java -version` writes its banner to stderr, not stdout — a
    // long-standing JDK quirk that predates stdout being the conventional
    // place for this kind of output.
    let text = String::from_utf8_lossy(&output.stderr);
    parse_java_major_version(&text)
}

/// Parses the major version out of a `java -version`-style banner line.
/// Handles both version schemes a supported JDK might report:
/// - **Modern (9+)**: `openjdk version "21.0.2" ...` / `java version "24" ...`
///   — the first dot/dash-separated component of the quoted version string
///   *is* the major version.
/// - **Legacy (8 and earlier)**: `java version "1.8.0_411"` — Java's old
///   `1.<major>.<minor>_<update>` scheme, where the *second* component is
///   what everyone actually calls the major version ("Java 8"), not the
///   leading `1`.
///
/// Returns `None` for anything that isn't a recognizable version banner at
/// all (no quoted version string, or a quoted string that doesn't start
/// with a number) — `probe_java` treats that identically to "candidate
/// doesn't exist" rather than erroring.
fn parse_java_major_version(text: &str) -> Option<u32> {
    let start = text.find('"')? + 1;
    let rest = &text[start..];
    let end = rest.find('"')?;
    let version = &rest[..end];

    let mut parts = version.split(['.', '_', '+', '-']);
    let first: u32 = parts.next()?.parse().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

/// `jdtls` has no project-local install convention of its own (Eclipse
/// ships it as a standalone launcher, not a per-project devDependency), so
/// its lookup order starts at `PATH`, same as `gopls`/`kotlin-lsp`.
fn lookup_jdtls() -> Option<LookupHit> {
    lookup_in_order(
        None,
        || which_on_path("jdtls"),
        || None,
        || mise_which("jdtls"),
        || install::installed_binary_path(&install::prefix_dir(), Language::Java),
    )
}

/// Where jdtls should keep its per-workspace index — the `-data` directory
/// in the launch line, without which jdtls falls back to its own default
/// (keyed off the workspace directory's *basename alone*, which collides
/// across two same-named directories at different paths — e.g. two
/// checkouts of the same repo, or an `app` module under two different
/// monorepos). Lives under [`crate::update::state_dir`] rather than
/// [`install::prefix_dir`] since a project's jdtls index is exactly the
/// kind of large, disposable, re-buildable-on-demand cache `$XDG_STATE_HOME`
/// is for, not the small downloaded-once server binaries `prefix_dir` holds.
/// `<hash>-<basename>`: the hash (over the *canonicalized* root, so two
/// paths to the same directory via a symlink still collide onto the same
/// index — the correct behavior, since it's the same project) is what
/// actually guarantees uniqueness; the appended basename is purely for a
/// human skimming `ls` output to recognize which directory is which —
/// nothing reads it back.
fn jdtls_data_dir(workspace_root: &Path) -> PathBuf {
    let canonical =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());
    // Hashes the path's raw OS bytes, not `to_string_lossy()`'s output —
    // lossy conversion collapses every invalid-UTF-8 subsequence to the
    // same U+FFFD placeholder, so two genuinely different non-UTF8 paths
    // (e.g. two sibling directories differing only in one invalid byte)
    // could otherwise hash identically and share one jdtls index directory.
    // `as_encoded_bytes()` (stable since Rust 1.74) is lossless on every
    // platform, unlike a `#[cfg(unix)]`-only `OsStrExt::as_bytes()`.
    let hash = fnv1a64(canonical.as_os_str().as_encoded_bytes());
    let basename = sanitize_basename(workspace_root);
    crate::update::state_dir()
        .join("jdtls-workspaces")
        .join(format!("{hash:016x}-{basename}"))
}

/// `workspace_root`'s final path component, with every character outside
/// `[A-Za-z0-9._-]` mapped to `_` — just enough to make it safe as one path
/// component appended after [`jdtls_data_dir`]'s hash, not a general-purpose
/// filename sanitizer. Falls back to a fixed placeholder for the
/// (essentially theoretical) case of a root with no final component at all
/// (e.g. `/`), since the hash prefix already guarantees the directory's
/// uniqueness on its own.
fn sanitize_basename(workspace_root: &Path) -> String {
    let raw = workspace_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workspace".to_owned());
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Hand-rolled FNV-1a 64 rather than `std::collections::hash_map::DefaultHasher`
/// — std explicitly does not guarantee `DefaultHasher`'s algorithm across
/// releases, but [`jdtls_data_dir`]'s hash needs to be stable *forever*: it
/// names an on-disk directory jdtls indexes a workspace under, and a value
/// that silently changed on some future Rust upgrade would orphan every
/// existing index (harmless in effect — jdtls just reindexes into a new
/// directory — but avoidable for free by not depending on an unstable
/// algorithm to begin with). About 8 lines; not worth a dependency for
/// something this small and this stable.
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = FNV_OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn ts_language_server_command(path: PathBuf) -> Command {
    let mut command = Command::new(path);
    command.arg("--stdio");
    command
}

fn pyright_command(path: PathBuf) -> Command {
    let mut command = Command::new(path);
    command.arg("--stdio");
    command
}

fn kotlin_lsp_command(path: PathBuf) -> Command {
    let mut command = Command::new(path);
    command.arg("--stdio");
    command
}

/// Which tier of [`lookup_in_order`] produced a hit, paired with the path
/// it found. [`ResolvedFrom`] without the `ConfigOverride` variant, which
/// never reaches this far — checked directly in `resolve_server`/`diagnose`
/// before any of this runs.
enum LookupHit {
    ProjectLocal(PathBuf),
    Path(PathBuf),
    ToolchainWhich(PathBuf),
    Mise(PathBuf),
    KatamariManaged(PathBuf),
}

impl LookupHit {
    fn into_path(self) -> PathBuf {
        match self {
            LookupHit::ProjectLocal(p)
            | LookupHit::Path(p)
            | LookupHit::ToolchainWhich(p)
            | LookupHit::Mise(p)
            | LookupHit::KatamariManaged(p) => p,
        }
    }

    fn source(&self) -> ResolvedFrom {
        match self {
            LookupHit::ProjectLocal(_) => ResolvedFrom::ProjectLocal,
            LookupHit::Path(_) => ResolvedFrom::Path,
            LookupHit::ToolchainWhich(_) => ResolvedFrom::ToolchainWhich,
            LookupHit::Mise(_) => ResolvedFrom::Mise,
            LookupHit::KatamariManaged(_) => ResolvedFrom::KatamariManaged,
        }
    }
}

/// The order every `lookup_*` function below checks in, common to all six
/// languages: a language-specific project-local convention (if any) beats
/// everything, then `PATH`, then a toolchain-specific fallback (`rustup
/// which` for rust-analyzer; nothing for the other five), then `mise
/// which` (cheap even when katamari itself wasn't invoked through mise —
/// see [`mise_which`]), and finally whatever [`crate::lsp::install`] has
/// already installed into katamari's own prefix. Factored as one function
/// over lookup closures — rather than each `lookup_*` duplicating the order
/// — so the order itself is unit-testable (see this module's tests) without
/// a real `PATH`, `mise`, or prefix directory in the picture, and so a
/// language-specific tier (like TypeScript/Python's project-local check)
/// can never accidentally skip one of the shared ones.
fn lookup_in_order(
    project_local: Option<PathBuf>,
    path_lookup: impl FnOnce() -> Option<PathBuf>,
    toolchain_lookup: impl FnOnce() -> Option<PathBuf>,
    mise_lookup: impl FnOnce() -> Option<PathBuf>,
    prefix_lookup: impl FnOnce() -> Option<PathBuf>,
) -> Option<LookupHit> {
    if let Some(path) = project_local.filter(|p| p.is_file()) {
        return Some(LookupHit::ProjectLocal(path));
    }
    path_lookup()
        .map(LookupHit::Path)
        .or_else(|| toolchain_lookup().map(LookupHit::ToolchainWhich))
        .or_else(|| mise_lookup().map(LookupHit::Mise))
        .or_else(|| prefix_lookup().map(LookupHit::KatamariManaged))
}

/// Tries `rust-analyzer` on `PATH` first (the common case: it's a rustup
/// component and rustup's shims put it on `PATH` directly), then falls back
/// to `rustup which rust-analyzer` — the form that works when katamari
/// itself is invoked through `mise exec --`, whose sandboxed `PATH` doesn't
/// always include rustup's shim directory even though `rustup` itself is
/// reachable.
fn lookup_rust_analyzer() -> Option<LookupHit> {
    lookup_in_order(
        None,
        || which_on_path("rust-analyzer"),
        || rustup_which("rust-analyzer"),
        || mise_which("rust-analyzer"),
        || install::installed_binary_path(&install::prefix_dir(), Language::Rust),
    )
}

/// Tries the project-local install first (`node_modules/.bin/...`, the
/// common case for a JS/TS project that lists it as a devDependency — a
/// project-pinned server version is more likely to agree with the project's
/// own `tsconfig.json` than whatever happens to be globally installed).
fn lookup_typescript_language_server(workspace_root: &Path) -> Option<LookupHit> {
    let local = workspace_root
        .join("node_modules")
        .join(".bin")
        .join("typescript-language-server");
    lookup_in_order(
        Some(local),
        || which_on_path("typescript-language-server"),
        || None,
        || mise_which("typescript-language-server"),
        || install::installed_binary_path(&install::prefix_dir(), Language::TypeScript),
    )
}

/// Tries the project-local virtualenv first (`.venv/bin/pyright-langserver`
/// — the common convention for a Python project's own interpreter and
/// dependencies).
fn lookup_pyright(workspace_root: &Path) -> Option<LookupHit> {
    let local = workspace_root
        .join(".venv")
        .join("bin")
        .join("pyright-langserver");
    lookup_in_order(
        Some(local),
        || which_on_path("pyright-langserver"),
        || None,
        || mise_which("pyright-langserver"),
        || install::installed_binary_path(&install::prefix_dir(), Language::Python),
    )
}

/// `gopls` has no meaningful project-local install location (it's a single
/// Go-toolchain binary, not a per-project dependency the way node/python
/// servers are), so its lookup order starts at `PATH`.
fn lookup_gopls() -> Option<LookupHit> {
    lookup_in_order(
        None,
        || which_on_path("gopls"),
        || None,
        || mise_which("gopls"),
        || install::installed_binary_path(&install::prefix_dir(), Language::Go),
    )
}

/// `kotlin-lsp` has no project-local install convention (JetBrains ships it
/// as a standalone archive, not a per-project devDependency), so its lookup
/// order starts at `PATH`, same as `gopls`.
fn lookup_kotlin_lsp() -> Option<LookupHit> {
    lookup_in_order(
        None,
        || which_on_path("kotlin-lsp"),
        || None,
        || mise_which("kotlin-lsp"),
        || install::installed_binary_path(&install::prefix_dir(), Language::Kotlin),
    )
}

/// Whether a go toolchain (needed to `go install golang.org/x/tools/gopls`)
/// is reachable at all — checked only to decide [`Unavailable::installable`]
/// for gopls specifically; see [`diagnose_language`]'s docs on why Go is the
/// one language this question matters for.
fn go_toolchain_available() -> bool {
    which_on_path("go").is_some() || mise_which("go").is_some()
}

/// The npm-strategy servers (`typescript-language-server`,
/// `pyright-langserver`) are `#!/usr/bin/env node` scripts: launched from
/// katamari's own managed install on a machine with no system Node — the
/// very machine `install::bootstrap_node` exists for — the shebang can't
/// resolve `node` and the server dies before speaking a byte of LSP (the
/// initialize failure surfaces only the shebang's `env:` error). Prepending
/// the bootstrapped runtime's bin directory to the child's `PATH` fixes
/// exactly that case; PATH/mise/project-local servers are left to the
/// environment that already resolved them.
fn prepend_bootstrapped_node_path(command: &mut Command) {
    let bin_dir = install::bootstrapped_node_bin_dir();
    if !bin_dir.is_dir() {
        return;
    }
    let mut paths = vec![bin_dir];
    if let Some(existing) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

/// `ktmr doctor`'s go-toolchain sub-check, mirroring [`java_jdk_status`]:
/// gopls resolves and initializes fine without a `go` binary, then fails
/// every real request with an opaque "no views" — a found-server row alone
/// would read as healthy while the one thing a user does (hover) breaks.
pub(crate) fn go_toolchain_status() -> (bool, String) {
    if let Some(path) = which_on_path("go").or_else(|| mise_which("go")) {
        (true, path.display().to_string())
    } else {
        (
            false,
            "not found — gopls needs a go toolchain at runtime (hover/definition will fail \
             with \"no views\" without one)"
                .to_owned(),
        )
    }
}

/// Builds every [`Unavailable`] in this module (and — via `pub(crate)` —
/// [`crate::lsp::manager`]'s custom-server-config-vanished-mid-session case
/// too) so the status-bar-message shape (`"LSP: {name} ✕ — {hint}"`) has
/// exactly one place it's ever spelled out, rather than each call site
/// hand-rolling the same `format!` and risking one silently drifting from
/// the rest if the shape ever changes.
pub(crate) fn unavailable(language_name: &str, hint: &str, installable: bool) -> Unavailable {
    Unavailable {
        reason: format!("LSP: {language_name} \u{2715} \u{2014} {hint}"),
        installable,
    }
}

pub(crate) fn which_on_path(bin: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var).find_map(|dir| {
        let candidate = dir.join(bin);
        candidate.is_file().then_some(candidate)
    })
}

fn rustup_which(bin: &str) -> Option<PathBuf> {
    let output = Command::new("rustup").args(["which", bin]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// `mise which <bin>` — the form that finds a mise-managed tool even when
/// its shim directory isn't on `PATH` (the same reasoning
/// [`lookup_rust_analyzer`]'s docs give for `rustup which`, generalized:
/// katamari itself is commonly invoked through `mise exec --`, whose
/// sandboxed `PATH` doesn't necessarily include every tool mise knows
/// about). A missing `mise` binary, or `mise` not knowing about `bin`, are
/// both just "not found" here — this is a cheap best-effort probe, not a
/// hard dependency on mise being installed at all. `pub(crate)` so
/// [`crate::lsp::install`]'s own `npm`/`go` toolchain lookups can share it
/// rather than re-implementing the same subprocess call.
pub(crate) fn mise_which(bin: &str) -> Option<PathBuf> {
    let output = Command::new("mise").args(["which", bin]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

/// Root-marker filenames shared by Kotlin and Java — both build via the
/// same Gradle/Maven ecosystem, so a Gradle settings file (or any of the
/// module-level build files) roots a mixed Kotlin/Java repo identically for
/// either language's server. Factored as one const, rather than two copies,
/// so the two languages' marker lists can never quietly drift apart —
/// `settings.gradle(.kts)` is listed alongside the module-level
/// `build.gradle(.kts)`/`pom.xml` markers so a directory with only a
/// settings file (no build script of its own — the common shape for a
/// multi-module Gradle root) still counts as tier 1's nearest marker, not
/// just tier 2's workspace marker (see [`is_workspace_root_marker`]).
const JVM_ROOT_MARKERS: &[&str] = &[
    "settings.gradle.kts",
    "settings.gradle",
    "build.gradle.kts",
    "build.gradle",
    "pom.xml",
];

/// One language's set of root-marker filenames, checked in order — the
/// first one found in a directory wins, but all are equally valid evidence
/// that directory is a workspace root for this language.
fn root_markers(language: Language) -> &'static [&'static str] {
    match language {
        Language::Rust => &["Cargo.toml"],
        Language::TypeScript => &["tsconfig.json", "jsconfig.json", "package.json"],
        Language::Python => &[
            "pyproject.toml",
            "setup.py",
            "setup.cfg",
            "requirements.txt",
        ],
        Language::Go => &["go.mod"],
        Language::Kotlin | Language::Java => JVM_ROOT_MARKERS,
    }
}

/// Finds a language server's root in two tiers, both bounded by `git_root`
/// (a workspace root for a file under review is never outside the
/// repository that file belongs to, even if some unrelated project happens
/// to sit further up the real filesystem tree):
///
/// 1. **Nearest marker** — walk up from `file`'s directory to the closest
///    ancestor containing one of `language`'s root markers (as before this
///    two-tier search existed).
/// 2. **Topmost workspace marker** — separately walk the same range for a
///    marker that declares its directory a *monorepo* workspace root (an
///    ancestor `Cargo.toml` with a `[workspace]` table, a `pnpm-workspace.yaml`
///    or `package.json` with a `"workspaces"` key, or a `go.work`). If one
///    exists, it wins over the nearest package-level marker, and if several
///    exist (nested workspaces), the one closest to `git_root` wins.
///
/// The second tier is what makes this monorepo-correct. Tier 1 alone picks
/// the *nearest* marker, which in a monorepo is almost always a member
/// package, not the workspace: a Cargo workspace member's own `Cargo.toml`
/// would win over the workspace root's, so opening `crates/a` and
/// `crates/b` in the same session spawns two independent rust-analyzers,
/// each redundantly indexing the whole workspace's dependency graph; every
/// npm/pnpm package has its own `package.json`, so each gets its own
/// typescript-language-server and cross-package go-to-definition never
/// connects. VSCode and rust-analyzer both root at the workspace for
/// exactly this reason — one server per monorepo, not one per package. When
/// no workspace-level marker exists anywhere in range, tier 2 finds
/// nothing and this function's behavior is identical to a plain
/// nearest-marker walk. Python is deliberately tier-1-only: pyright has no
/// comparable workspace-of-workspaces concept to key off here.
///
/// Falls back to `git_root` itself when neither tier finds anything, rather
/// than `None` — most language servers work fine pointed at a directory
/// with no project file (rust-analyzer being the outlier that actually
/// needs `Cargo.toml`, which is why *its* root marker is checked first and
/// would already have matched if one existed).
pub fn workspace_root(file: &Path, git_root: &Path, language: Language) -> PathBuf {
    let Some(start) = file.parent() else {
        return git_root.to_path_buf();
    };
    if let Some(workspace) = topmost_workspace_marker_dir(start, git_root, language) {
        return workspace;
    }
    nearest_marker_dir(start, git_root, language).unwrap_or_else(|| git_root.to_path_buf())
}

/// Tier 1 of [`workspace_root`]: the closest ancestor of `start` (bounded by
/// `git_root`) containing one of `language`'s package-level root markers,
/// or `None` if there isn't one in range.
fn nearest_marker_dir(start: &Path, git_root: &Path, language: Language) -> Option<PathBuf> {
    nearest_dir_containing_any(start, git_root, root_markers(language))
}

/// The nearest-ancestor-marker walk itself, factored out of
/// [`nearest_marker_dir`] so [`custom_workspace_root`] (a *custom*
/// `[lsp.servers.<id>]` entry's user-supplied `root_markers`, rather than
/// one of [`root_markers`]'s built-in, per-[`Language`] lists) can reuse the
/// exact same walk instead of a second copy that could drift from it.
fn nearest_dir_containing_any(start: &Path, git_root: &Path, markers: &[&str]) -> Option<PathBuf> {
    let mut dir = start;
    loop {
        if markers.iter().any(|marker| dir.join(marker).is_file()) {
            return Some(dir.to_path_buf());
        }
        if dir == git_root {
            return None;
        }
        dir = dir.parent()?;
    }
}

/// [`workspace_root`]'s counterpart for a `LangKey::Custom` server: unlike a
/// built-in language, a custom id has no adapter-specific tier-2
/// "workspace of workspaces" logic (no notion of a Cargo `[workspace]`
/// table, a `go.work`, or a Gradle `settings.gradle` for a server this
/// module has never heard of) — just tier 1's plain nearest-ancestor walk
/// over the entry's own `root_markers`, reusing
/// [`nearest_dir_containing_any`] so the walk itself can never drift from
/// the built-in one. Falls back to `git_root` both when `root_markers` is
/// empty (the field's documented default-to-git-root behavior — see
/// [`crate::config::ServerOverride::root_markers`]) and when it's non-empty
/// but nothing in range matched, exactly mirroring [`workspace_root`]'s own
/// git-root fallback.
pub fn custom_workspace_root(file: &Path, git_root: &Path, root_markers: &[String]) -> PathBuf {
    if root_markers.is_empty() {
        return git_root.to_path_buf();
    }
    let Some(start) = file.parent() else {
        return git_root.to_path_buf();
    };
    let markers: Vec<&str> = root_markers.iter().map(String::as_str).collect();
    nearest_dir_containing_any(start, git_root, &markers).unwrap_or_else(|| git_root.to_path_buf())
}

/// Tier 2 of [`workspace_root`]: scans every ancestor of `start` up to and
/// including `git_root` for a workspace-level marker (see
/// [`is_workspace_root_marker`]), returning the topmost match. Walks the
/// full range rather than stopping at the first hit — nested workspaces are
/// rare but not invalid, and "closest to `git_root`" is the one whose
/// server would actually cover every package underneath it, so a match
/// found later in the ascent (closer to `git_root`) always overwrites an
/// earlier, nearer one.
fn topmost_workspace_marker_dir(
    start: &Path,
    git_root: &Path,
    language: Language,
) -> Option<PathBuf> {
    let mut topmost = None;
    let mut dir = start;
    loop {
        if is_workspace_root_marker(dir, language) {
            topmost = Some(dir.to_path_buf());
        }
        if dir == git_root {
            break;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => break,
        }
    }
    topmost
}

/// Whether `dir` itself declares a monorepo workspace root for `language`.
/// Unreadable or unparseable marker files are treated as absent rather than
/// erroring — a malformed `Cargo.toml`/`package.json` somewhere above the
/// file being viewed shouldn't break root resolution for it; it just falls
/// through to tier 1's nearest-marker behavior, same as if the file weren't
/// there at all.
fn is_workspace_root_marker(dir: &Path, language: Language) -> bool {
    match language {
        Language::Rust => cargo_toml_declares_workspace(&dir.join("Cargo.toml")),
        Language::TypeScript => {
            dir.join("pnpm-workspace.yaml").is_file()
                || package_json_declares_workspaces(&dir.join("package.json"))
        }
        Language::Go => dir.join("go.work").is_file(),
        Language::Python => false,
        // `settings.gradle(.kts)` is unconditionally the top of a Gradle
        // build — unlike Cargo/npm's workspace markers, it needs no content
        // check (a Gradle project either has one declaring its modules, or
        // it's a single-module project with no workspace tier at all), so
        // this mirrors `go.work`'s presence-only check rather than
        // `cargo_toml_declares_workspace`'s content-parsing one. Shared by
        // Kotlin and Java for the same reason [`JVM_ROOT_MARKERS`] is: one
        // Gradle settings file roots a mixed Kotlin/Java module tree
        // identically for either language's server.
        Language::Kotlin | Language::Java => is_gradle_settings_marker(dir),
    }
}

/// Whether `dir` itself is a Gradle multi-module build's settings root —
/// factored out of [`is_workspace_root_marker`]'s match so Kotlin and Java
/// share one implementation rather than two copies that could drift.
fn is_gradle_settings_marker(dir: &Path) -> bool {
    dir.join("settings.gradle.kts").is_file() || dir.join("settings.gradle").is_file()
}

/// True if `path` parses as TOML and has a top-level `[workspace]` table —
/// note this is also true for a workspace root that's simultaneously a
/// member crate (a root package with `[workspace]` *and* `[package]` in the
/// same file, a common Cargo layout); that's fine, since such a directory
/// is correctly both the nearest marker and the workspace marker.
fn cargo_toml_declares_workspace(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return false;
    };
    table.contains_key("workspace")
}

/// True if `path` parses as JSON and has a top-level `"workspaces"` key —
/// the npm/yarn/pnpm convention for listing member package globs, checked
/// only for presence (its value's shape isn't this function's concern).
fn package_json_declares_workspaces(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    value
        .as_object()
        .is_some_and(|obj| obj.contains_key("workspaces"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_each_language_by_extension() {
        assert_eq!(
            Language::detect(Path::new("src/main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(
            Language::detect(Path::new("src/app.tsx")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::detect(Path::new("src/app.jsx")),
            Some(Language::TypeScript)
        );
        assert_eq!(
            Language::detect(Path::new("script.py")),
            Some(Language::Python)
        );
        assert_eq!(Language::detect(Path::new("main.go")), Some(Language::Go));
        assert_eq!(
            Language::detect(Path::new("src/Main.kt")),
            Some(Language::Kotlin)
        );
        assert_eq!(
            Language::detect(Path::new("build.gradle.kts")),
            Some(Language::Kotlin)
        );
        assert_eq!(
            Language::detect(Path::new("src/Main.java")),
            Some(Language::Java)
        );
        assert_eq!(Language::detect(Path::new("README.md")), None);
        assert_eq!(Language::detect(Path::new("no_extension")), None);
    }

    #[test]
    fn java_lsp_id_is_java() {
        assert_eq!(Language::Java.lsp_id(), "java");
    }

    #[test]
    fn lsp_language_id_distinguishes_the_typescript_family_by_extension() {
        assert_eq!(
            lsp_language_id(Language::TypeScript, Path::new("a.ts")),
            "typescript"
        );
        assert_eq!(
            lsp_language_id(Language::TypeScript, Path::new("a.tsx")),
            "typescriptreact"
        );
        assert_eq!(
            lsp_language_id(Language::TypeScript, Path::new("a.js")),
            "javascript"
        );
        assert_eq!(
            lsp_language_id(Language::TypeScript, Path::new("a.jsx")),
            "javascriptreact"
        );
        assert_eq!(lsp_language_id(Language::Rust, Path::new("a.rs")), "rust");
    }

    #[test]
    fn workspace_root_finds_the_nearest_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let crate_dir = repo_root.join("crates").join("foo");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\n").unwrap();
        let file = crate_dir.join("src").join("lib.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(workspace_root(&file, repo_root, Language::Rust), crate_dir);
    }

    #[test]
    fn workspace_root_falls_back_to_git_root_when_no_marker_before_it() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join("notes")).unwrap();
        let file = repo_root.join("notes").join("todo.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Rust),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_checks_git_root_itself_before_giving_up() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("Cargo.toml"), "[package]\n").unwrap();
        let file = repo_root.join("src").join("main.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Rust),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_finds_typescript_markers() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let project_dir = repo_root.join("packages").join("web");
        std::fs::create_dir_all(project_dir.join("src")).unwrap();
        std::fs::write(project_dir.join("tsconfig.json"), "{}").unwrap();
        let file = project_dir.join("src").join("index.ts");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::TypeScript),
            project_dir
        );
    }

    #[test]
    fn workspace_root_finds_python_markers() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("pyproject.toml"), "").unwrap();
        let file = repo_root.join("pkg").join("mod.py");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Python),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_finds_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("go.mod"), "module example\n").unwrap();
        let file = repo_root.join("cmd").join("main.go");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Go),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_prefers_the_cargo_workspace_root_over_a_members_own_cargo_toml() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/a\"]\n",
        )
        .unwrap();
        let crate_dir = repo_root.join("crates").join("a");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\nname = \"a\"\n").unwrap();
        let file = crate_dir.join("src").join("lib.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Rust),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_finds_npm_workspaces_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("package.json"),
            r#"{"workspaces":["packages/*"]}"#,
        )
        .unwrap();
        let project_dir = repo_root.join("packages").join("web");
        std::fs::create_dir_all(project_dir.join("src")).unwrap();
        std::fs::write(project_dir.join("package.json"), "{}").unwrap();
        std::fs::write(project_dir.join("tsconfig.json"), "{}").unwrap();
        let file = project_dir.join("src").join("index.ts");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::TypeScript),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_finds_pnpm_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("pnpm-workspace.yaml"),
            "packages:\n  - packages/*\n",
        )
        .unwrap();
        let project_dir = repo_root.join("packages").join("web");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("package.json"), "{}").unwrap();
        let file = project_dir.join("index.ts");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::TypeScript),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_does_not_mistake_a_plain_package_json_for_a_workspace_root() {
        // A `package.json` at the repo root with no `"workspaces"` key isn't
        // a workspace marker — the nearest marker (the nested project's own
        // `tsconfig.json`) must still win, same as before this feature
        // existed. This guards against a false-positive that would collapse
        // every independent TS project sharing a git repo into one server.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("package.json"),
            r#"{"name":"monorepo-tools"}"#,
        )
        .unwrap();
        let project_dir = repo_root.join("packages").join("web");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("tsconfig.json"), "{}").unwrap();
        let file = project_dir.join("index.ts");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::TypeScript),
            project_dir
        );
    }

    #[test]
    fn workspace_root_finds_go_work_root_over_a_nested_go_mod() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("go.work"), "go 1.22\n\nuse ./services/api\n").unwrap();
        let service_dir = repo_root.join("services").join("api");
        std::fs::create_dir_all(&service_dir).unwrap();
        std::fs::write(service_dir.join("go.mod"), "module api\n").unwrap();
        let file = service_dir.join("main.go");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Go),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_finds_the_nearest_kotlin_build_gradle_kts() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let module_dir = repo_root.join("app");
        std::fs::create_dir_all(module_dir.join("src").join("main").join("kotlin")).unwrap();
        std::fs::write(module_dir.join("build.gradle.kts"), "").unwrap();
        let file = module_dir
            .join("src")
            .join("main")
            .join("kotlin")
            .join("Main.kt");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Kotlin),
            module_dir
        );
    }

    #[test]
    fn workspace_root_prefers_the_gradle_settings_root_over_a_modules_build_gradle_kts() {
        // Mirrors `workspace_root_prefers_the_cargo_workspace_root_over_a_members_own_cargo_toml`:
        // a multi-module Gradle build's `settings.gradle.kts` at the repo
        // root should win over a nearer module's own `build.gradle.kts`, so
        // kotlin-lsp sees every module's classpath from one server rather
        // than one server per module.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("settings.gradle.kts"),
            "rootProject.name = \"demo\"\ninclude(\"app\")\n",
        )
        .unwrap();
        let module_dir = repo_root.join("app");
        std::fs::create_dir_all(module_dir.join("src")).unwrap();
        std::fs::write(module_dir.join("build.gradle.kts"), "").unwrap();
        let file = module_dir.join("src").join("Main.kt");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Kotlin),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_finds_a_kotlin_pom_xml_when_no_gradle_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("pom.xml"), "").unwrap();
        let file = repo_root.join("src").join("main").join("Main.kt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Kotlin),
            repo_root.to_path_buf()
        );
    }

    // --- Java shares Kotlin's JVM_ROOT_MARKERS / Gradle-settings logic ---

    #[test]
    fn workspace_root_finds_a_java_pom_xml_when_no_gradle_marker_exists() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("pom.xml"), "").unwrap();
        let file = repo_root
            .join("src")
            .join("main")
            .join("java")
            .join("Main.java");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Java),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_prefers_the_gradle_settings_root_over_a_java_modules_build_gradle_kts() {
        // Mirrors `workspace_root_prefers_the_gradle_settings_root_over_a_modules_build_gradle_kts`
        // for Java, confirming `JVM_ROOT_MARKERS`/`is_gradle_settings_marker`
        // sharing didn't quietly change Kotlin's own precedence.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("settings.gradle.kts"),
            "rootProject.name = \"demo\"\ninclude(\"app\")\n",
        )
        .unwrap();
        let module_dir = repo_root.join("app");
        std::fs::create_dir_all(module_dir.join("src")).unwrap();
        std::fs::write(module_dir.join("build.gradle.kts"), "").unwrap();
        let file = module_dir.join("src").join("Main.java");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Java),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_picks_the_topmost_workspace_marker_when_nested() {
        // An outer Cargo workspace containing an inner one (unusual, but not
        // invalid) — the server covering the whole monorepo should win, not
        // the inner sub-workspace.
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(
            repo_root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"nested\"]\n",
        )
        .unwrap();
        let nested_root = repo_root.join("nested");
        std::fs::create_dir_all(nested_root.join("crates").join("b").join("src")).unwrap();
        std::fs::write(
            nested_root.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/b\"]\n",
        )
        .unwrap();
        std::fs::write(
            nested_root.join("crates").join("b").join("Cargo.toml"),
            "[package]\n",
        )
        .unwrap();
        let file = nested_root
            .join("crates")
            .join("b")
            .join("src")
            .join("lib.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::Rust),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn workspace_root_treats_a_malformed_cargo_toml_ancestor_as_non_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        // Not valid TOML at all — must be skipped, not panic, falling
        // through to the nearest marker (the crate's own `Cargo.toml`).
        std::fs::write(repo_root.join("Cargo.toml"), "this is not valid { toml").unwrap();
        let crate_dir = repo_root.join("crates").join("a");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(crate_dir.join("Cargo.toml"), "[package]\n").unwrap();
        let file = crate_dir.join("src").join("lib.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(workspace_root(&file, repo_root, Language::Rust), crate_dir);
    }

    #[test]
    fn workspace_root_treats_a_malformed_package_json_ancestor_as_non_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::write(repo_root.join("package.json"), "{not valid json").unwrap();
        let project_dir = repo_root.join("packages").join("web");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("tsconfig.json"), "{}").unwrap();
        let file = project_dir.join("index.ts");
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            workspace_root(&file, repo_root, Language::TypeScript),
            project_dir
        );
    }

    #[test]
    fn resolve_server_reports_a_hint_when_a_binary_is_missing() {
        // Exercised against a PATH-less environment isn't practical here, but
        // gopls/pyright/typescript-language-server are not expected to be on
        // this workspace's PATH by default in CI — if resolution *does*
        // succeed (a developer happens to have it installed globally),
        // that's fine too; only the failure message's shape is asserted.
        let overrides = HashMap::new();
        if let Err(unavailable) = resolve_server(Language::Go, Path::new("/repo"), &overrides) {
            assert!(unavailable.reason.contains("gopls"));
            assert!(unavailable.reason.starts_with("LSP: go"));
        }
    }

    #[test]
    fn resolve_server_reports_a_hint_when_kotlin_lsp_is_missing() {
        // Unlike gopls, kotlin-lsp is unconditionally installable when
        // missing (it bundles its own JVM — see `diagnose_language`'s
        // docs), so this only asserts the hint's shape, not `installable`.
        let overrides = HashMap::new();
        if let Err(unavailable) = resolve_server(Language::Kotlin, Path::new("/repo"), &overrides) {
            assert!(unavailable.reason.contains("kotlin-lsp"));
            assert!(unavailable.reason.starts_with("LSP: kotlin"));
            assert!(unavailable.installable);
        }
    }

    #[test]
    fn resolve_server_reports_a_hint_when_java_is_unavailable() {
        // Java's hint has three possible shapes depending on this host's
        // JDK state (see `java_install_hint`'s docs), unlike the flat
        // gopls/kotlin-lsp hints above — so `installable` isn't asserted
        // one way here; only the hint's shape (naming either a JDK or
        // jdtls itself) is checked, tolerant of whatever this CI host
        // happens to have installed.
        let overrides = HashMap::new();
        if let Err(unavailable) = resolve_server(Language::Java, Path::new("/repo"), &overrides) {
            assert!(unavailable.reason.starts_with("LSP: java"));
            assert!(
                unavailable.reason.contains("JDK") || unavailable.reason.contains("jdtls"),
                "unexpected hint: {}",
                unavailable.reason
            );
        }
    }

    #[test]
    fn java_jdk_note_reports_a_jdk_or_not_found_tolerant_of_this_hosts_java_state() {
        // Same tolerance as the hint test above: this can't control
        // whether the CI host has a JDK, so it only checks the note's
        // shape (a path naming a Java version, or the literal "not
        // found"), not which of the three `JavaProbe` branches fired.
        let note = java_jdk_note();
        assert!(
            note == "not found" || note.contains("Java"),
            "unexpected note: {note}"
        );
    }

    #[test]
    fn java_jdk_status_note_matches_java_jdk_note_and_ok_flag_matches_a_fresh_probe() {
        // Pins that `java_jdk_note` is now a thin wrapper over
        // `java_jdk_status` (same text either way) and that the `ok` half
        // agrees with `probe_java`'s own `Ok` variant — the exact pairing
        // `doctor.rs`'s Java sub-check relies on to never tag a "not found"/
        // "too old" note as `ok`.
        let (ok, note) = java_jdk_status();
        assert_eq!(java_jdk_note(), note);
        assert_eq!(ok, matches!(probe_java(), JavaProbe::Ok(_)));
    }

    #[test]
    fn a_config_override_takes_priority_over_the_built_in_lookup() {
        let overrides = HashMap::from([(
            "go".to_owned(),
            ServerOverride {
                command: "/opt/bin/my-gopls".to_owned(),
                args: vec!["--stdio".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        let resolved = resolve_server(Language::Go, Path::new("/repo"), &overrides).unwrap();
        assert_eq!(resolved.command.get_program(), "/opt/bin/my-gopls");
        assert_eq!(
            resolved.command.get_args().collect::<Vec<_>>(),
            vec!["--stdio"]
        );
        assert_eq!(resolved.initialization_options, None);
    }

    #[test]
    fn a_built_in_override_also_passes_through_initialization_options() {
        // Closes the gap the design docs call out: previously only a
        // katamari-managed typescript-language-server got
        // `initialization_options` at all (hardcoded `None` for every
        // config-override resolution) — now a built-in override like this
        // `rust` one can send rust-analyzer settings too.
        let overrides = HashMap::from([(
            "rust".to_owned(),
            ServerOverride {
                command: "/opt/bin/rust-analyzer".to_owned(),
                initialization_options: Some(toml::Value::Table(
                    toml::toml! { check = { command = "clippy" } },
                )),
                ..ServerOverride::default()
            },
        )]);
        let resolved = resolve_server(Language::Rust, Path::new("/repo"), &overrides).unwrap();
        let options = resolved.initialization_options.unwrap();
        assert_eq!(options["check"]["command"], serde_json::json!("clippy"));
    }

    #[test]
    fn no_override_falls_through_to_the_built_in_lookup() {
        // No "go" entry in `overrides` — resolution falls through to the
        // built-in gopls lookup, matching
        // `resolve_server_reports_a_hint_when_a_binary_is_missing`'s own
        // caveat about this workspace's PATH.
        let overrides = HashMap::new();
        let result = resolve_server(Language::Go, Path::new("/repo"), &overrides);
        if let Err(unavailable) = result {
            assert!(unavailable.reason.starts_with("LSP: go"));
        }
    }

    // --- lookup_in_order: the shared resolution-order primitive ----------

    #[test]
    fn lookup_in_order_prefers_path_over_katamari_managed() {
        let hit = lookup_in_order(
            None,
            || Some(PathBuf::from("/usr/bin/somebin")),
            || None,
            || None,
            || Some(PathBuf::from("/prefix/somebin")),
        );
        assert!(
            matches!(hit, Some(LookupHit::Path(p)) if p.as_path() == Path::new("/usr/bin/somebin"))
        );
    }

    #[test]
    fn lookup_in_order_falls_back_to_katamari_managed_when_nothing_else_matches() {
        // Nothing on PATH, no toolchain-specific fallback, no mise — a
        // binary katamari already installed into its own prefix still gets
        // used, without anything having to trigger a fresh install (that
        // trigger only ever fires when this whole chain returns `None`).
        let hit = lookup_in_order(
            None,
            || None,
            || None,
            || None,
            || Some(PathBuf::from("/prefix/somebin")),
        );
        assert!(
            matches!(hit, Some(LookupHit::KatamariManaged(p)) if p.as_path() == Path::new("/prefix/somebin"))
        );
    }

    #[test]
    fn lookup_in_order_prefers_project_local_over_everything() {
        let hit = lookup_in_order(
            Some(PathBuf::from(file!())), // any real file this test binary can see
            || Some(PathBuf::from("/usr/bin/somebin")),
            || None,
            || None,
            || Some(PathBuf::from("/prefix/somebin")),
        );
        assert!(matches!(hit, Some(LookupHit::ProjectLocal(_))));
    }

    #[test]
    fn lookup_in_order_is_none_when_every_tier_misses() {
        let hit = lookup_in_order(None, || None, || None, || None, || None);
        assert!(hit.is_none());
    }

    #[test]
    fn mise_lookup_beats_katamari_managed_but_loses_to_path() {
        let hit = lookup_in_order(
            None,
            || None,
            || None,
            || Some(PathBuf::from("/mise/shims/somebin")),
            || Some(PathBuf::from("/prefix/somebin")),
        );
        assert!(
            matches!(hit, Some(LookupHit::Mise(p)) if p.as_path() == Path::new("/mise/shims/somebin"))
        );
    }

    // --- Java JDK resolution / jdtls launch-line assembly -----------------

    #[test]
    fn parse_java_major_version_handles_the_modern_scheme() {
        assert_eq!(
            parse_java_major_version(
                "openjdk version \"21.0.2\" 2024-01-16\nOpenJDK Runtime Environment (build 21.0.2+13)"
            ),
            Some(21)
        );
        assert_eq!(
            parse_java_major_version(
                "java version \"24\" 2026-03-17\nJava(TM) SE Runtime Environment"
            ),
            Some(24)
        );
        assert_eq!(
            parse_java_major_version("openjdk version \"17.0.9+11-Ubuntu\""),
            Some(17)
        );
    }

    #[test]
    fn parse_java_major_version_handles_the_legacy_1_dot_n_scheme() {
        assert_eq!(
            parse_java_major_version(
                "java version \"1.8.0_411\"\nJava(TM) SE Runtime Environment (build 1.8.0_411-b11)"
            ),
            Some(8)
        );
    }

    #[test]
    fn parse_java_major_version_returns_none_for_unparseable_input() {
        assert_eq!(parse_java_major_version(""), None);
        assert_eq!(
            parse_java_major_version("bash: java: command not found"),
            None
        );
        assert_eq!(parse_java_major_version("java version \"nonsense\""), None);
    }

    #[test]
    fn jdtls_config_dir_name_maps_every_supported_os() {
        assert_eq!(jdtls_config_dir_name("linux").unwrap(), "config_linux");
        assert_eq!(jdtls_config_dir_name("macos").unwrap(), "config_mac");
        assert_eq!(jdtls_config_dir_name("windows").unwrap(), "config_win");
    }

    #[test]
    fn jdtls_config_dir_name_reports_unsupported_targets_clearly() {
        let err = jdtls_config_dir_name("freebsd").unwrap_err();
        assert!(err.contains("freebsd"));
    }

    #[test]
    fn fnv1a64_matches_known_fnv_test_vectors() {
        // Golden values from the FNV reference test vectors — pins this
        // hand-rolled implementation against the standard algorithm forever,
        // the whole point of not depending on `DefaultHasher` (see
        // `fnv1a64`'s docs).
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn jdtls_data_dir_is_deterministic_for_the_same_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(jdtls_data_dir(dir.path()), jdtls_data_dir(dir.path()));
    }

    #[test]
    fn jdtls_data_dir_differs_for_different_workspace_roots() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        assert_ne!(jdtls_data_dir(dir_a.path()), jdtls_data_dir(dir_b.path()));
    }

    #[test]
    #[cfg(unix)]
    fn jdtls_data_dir_does_not_collide_two_distinct_non_utf8_workspace_roots() {
        // Two sibling directories differing only by one invalid-UTF-8 byte
        // (both valid POSIX filenames) used to `to_string_lossy()` into the
        // identical U+FFFD-substituted string, hashing identically and
        // colliding onto the same jdtls index directory — hashing the raw
        // OS bytes instead must tell them apart.
        use std::os::unix::ffi::OsStrExt;

        let parent = tempfile::tempdir().unwrap();
        let dir_a = parent.path().join(std::ffi::OsStr::from_bytes(b"proj\xFF"));
        let dir_b = parent.path().join(std::ffi::OsStr::from_bytes(b"proj\xFE"));
        std::fs::create_dir(&dir_a).unwrap();
        std::fs::create_dir(&dir_b).unwrap();

        // Confirms the premise: both names really do collapse to the same
        // lossy string, so a passing test here is actually exercising the
        // fix, not a difference `to_string_lossy()` would've caught anyway.
        assert_eq!(dir_a.to_string_lossy(), dir_b.to_string_lossy());

        assert_ne!(jdtls_data_dir(&dir_a), jdtls_data_dir(&dir_b));
    }

    #[test]
    fn jdtls_data_dir_lives_under_the_jdtls_workspaces_subdirectory_and_keeps_the_basename() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("my-repo");
        std::fs::create_dir_all(&workspace).unwrap();

        let data_dir = jdtls_data_dir(&workspace);
        assert!(data_dir.parent().unwrap().ends_with("jdtls-workspaces"));
        assert!(
            data_dir
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-my-repo")
        );
    }

    #[test]
    fn sanitize_basename_replaces_unsafe_characters() {
        assert_eq!(
            sanitize_basename(Path::new("/tmp/my repo (v2)")),
            "my_repo__v2_"
        );
        assert_eq!(
            sanitize_basename(Path::new("/tmp/plain-name_1.2")),
            "plain-name_1.2"
        );
    }

    /// The flag sequence common to every `jdtls_managed_launch_args` call
    /// regardless of Java major version — from `-Declipse.application`
    /// through the trailing `-data <data_dir>` — factored out so both the
    /// major=21 and major=24+ tests below can assert the *entire* argv
    /// (not just the handful of elements each version's launch line differs
    /// on) without duplicating this ~15-element chain twice. `config_linux`
    /// is hardcoded rather than routed through `jdtls_config_dir_name` a
    /// second time because these tests only run where
    /// `std::env::consts::OS` is what CI actually runs on (linux);
    /// `jdtls_config_dir_name_maps_every_supported_os` above covers the
    /// other two OSes independently.
    fn standard_jdtls_launch_tail(install_dir: &Path, data_dir: &Path) -> Vec<OsString> {
        [
            "-Declipse.application=org.eclipse.jdt.ls.core.id1",
            "-Dosgi.bundles.defaultStartLevel=4",
            "-Declipse.product=org.eclipse.jdt.ls.core.product",
            "-Dosgi.checkConfiguration=true",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(std::iter::once(OsString::from(format!(
            "-Dosgi.sharedConfiguration.area={}",
            install_dir.join("config_linux").display()
        ))))
        .chain(
            [
                "-Dosgi.sharedConfiguration.area.readOnly=true",
                "-Dosgi.configuration.cascaded=true",
                "-Xms1G",
                "--add-modules=ALL-SYSTEM",
                "--add-opens",
                "java.base/java.util=ALL-UNNAMED",
                "--add-opens",
                "java.base/java.lang=ALL-UNNAMED",
                "-jar",
            ]
            .into_iter()
            .map(OsString::from),
        )
        .chain(std::iter::once(
            install_dir
                .join("plugins")
                .join(install::JDTLS_LAUNCHER_JAR)
                .into_os_string(),
        ))
        .chain(["-data"].into_iter().map(OsString::from))
        .chain(std::iter::once(data_dir.as_os_str().to_owned()))
        .collect()
    }

    #[test]
    fn jdtls_managed_launch_args_matches_jdtls_pys_real_launch_line() {
        // Mirrors `bin/jdtls.py`'s own launch line verbatim (verified by
        // hand against the real jdtls 1.60.0 tarball) — this test is the
        // guard against a future refactor silently reordering or dropping
        // one of those flags.
        let java_path = Path::new("/opt/java21/bin/java");
        let install_dir = Path::new("/prefix/jdt-language-server/1.60.0");
        let data_dir = Path::new("/state/jdtls-workspaces/abc123-myrepo");

        let args = jdtls_managed_launch_args(java_path, 21, install_dir, data_dir).unwrap();

        let expected: Vec<OsString> = std::iter::once(OsString::from("/opt/java21/bin/java"))
            .chain(standard_jdtls_launch_tail(install_dir, data_dir))
            .collect();

        assert_eq!(args, expected);
    }

    #[test]
    fn jdtls_managed_launch_args_prepends_xml_entity_flags_for_java_24_and_above() {
        // Asserts the *full* expected argv, not just the two XML-entity
        // flags and their immediate successor — a bug that reorders, drops,
        // or duplicates one of the trailing flags (e.g. one of the
        // `--add-opens` pairs, or the `-jar`/`-data` pair) specifically
        // inside the major>=24 branch must fail this test, not just the
        // major=21 one above, which never exercises that branch at all.
        let java_path = Path::new("/java");
        let install_dir = Path::new("/prefix/jdt-language-server/1.60.0");
        let data_dir = Path::new("/data");

        let args = jdtls_managed_launch_args(java_path, 24, install_dir, data_dir).unwrap();

        let expected: Vec<OsString> = [
            "/java",
            "-Djdk.xml.maxGeneralEntitySizeLimit=0",
            "-Djdk.xml.totalEntitySizeLimit=0",
        ]
        .into_iter()
        .map(OsString::from)
        .chain(standard_jdtls_launch_tail(install_dir, data_dir))
        .collect();

        assert_eq!(args, expected);
    }

    #[test]
    fn jdtls_managed_launch_args_omits_xml_entity_flags_below_java_24() {
        let args = jdtls_managed_launch_args(
            Path::new("/java"),
            23,
            Path::new("/prefix/jdt-language-server/1.60.0"),
            Path::new("/data"),
        )
        .unwrap();

        assert_eq!(
            args[1],
            OsString::from("-Declipse.application=org.eclipse.jdt.ls.core.id1"),
            "no XML-entity flags expected below Java 24: {args:?}"
        );
    }

    // --- LangKey / custom servers (Part 2) --------------------------------

    #[test]
    fn lang_key_detect_prefers_a_built_in_language_over_any_custom_claim() {
        // A custom entry claiming `.rs` (nonsensical, but not this
        // function's job to forbid — `custom_extension_map` is what drops
        // it, with a warning) must still lose to the built-in `Language::Rust`
        // here: `LangKey::detect` always checks built-in first.
        let custom = HashMap::from([("rs".to_owned(), "not-rust-analyzer".to_owned())]);
        assert_eq!(
            LangKey::detect(Path::new("main.rs"), &custom),
            Some(LangKey::Builtin(Language::Rust))
        );
    }

    #[test]
    fn lang_key_detect_falls_through_to_a_custom_extension_map_hit() {
        let custom = HashMap::from([("rb".to_owned(), "ruby".to_owned())]);
        assert_eq!(
            LangKey::detect(Path::new("app.rb"), &custom),
            Some(LangKey::Custom("ruby".to_owned()))
        );
    }

    #[test]
    fn lang_key_detect_is_none_for_an_extension_nothing_claims() {
        let custom = HashMap::from([("rb".to_owned(), "ruby".to_owned())]);
        assert_eq!(LangKey::detect(Path::new("notes.md"), &custom), None);
        assert_eq!(LangKey::detect(Path::new("no_extension"), &custom), None);
    }

    #[test]
    fn lang_key_display_matches_lsp_id_for_builtin_and_the_id_for_custom() {
        assert_eq!(LangKey::Builtin(Language::Go).to_string(), "go");
        assert_eq!(LangKey::Custom("ruby".to_owned()).to_string(), "ruby");
    }

    #[test]
    fn lsp_language_id_for_builtin_delegates_to_lsp_language_id() {
        let overrides = HashMap::new();
        assert_eq!(
            lsp_language_id_for(
                &LangKey::Builtin(Language::TypeScript),
                Path::new("a.tsx"),
                &overrides
            ),
            "typescriptreact"
        );
    }

    #[test]
    fn lsp_language_id_for_custom_uses_the_configured_override_or_falls_back_to_the_id() {
        let overrides = HashMap::from([
            (
                "ruby-lsp".to_owned(),
                ServerOverride {
                    command: "ruby-lsp".to_owned(),
                    language_id: Some("ruby".to_owned()),
                    ..ServerOverride::default()
                },
            ),
            (
                "solargraph".to_owned(),
                ServerOverride {
                    command: "solargraph".to_owned(),
                    ..ServerOverride::default()
                },
            ),
        ]);
        assert_eq!(
            lsp_language_id_for(
                &LangKey::Custom("ruby-lsp".to_owned()),
                Path::new("app.rb"),
                &overrides
            ),
            "ruby"
        );
        assert_eq!(
            lsp_language_id_for(
                &LangKey::Custom("solargraph".to_owned()),
                Path::new("app.rb"),
                &overrides
            ),
            "solargraph",
            "no `language_id` override: falls back to the id itself"
        );
    }

    #[test]
    fn custom_extension_map_collects_every_claimed_extension() {
        let overrides = HashMap::from([(
            "ruby".to_owned(),
            ServerOverride {
                command: "solargraph".to_owned(),
                extensions: vec!["rb".to_owned(), "erb".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        let map = custom_extension_map(&overrides);
        assert_eq!(map.get("rb").map(String::as_str), Some("ruby"));
        assert_eq!(map.get("erb").map(String::as_str), Some("ruby"));
    }

    #[test]
    fn custom_extension_map_ignores_entries_with_no_extensions() {
        // A plain built-in override (e.g. `[lsp.servers.rust]` with just
        // `command`) leaves `extensions` empty — it must not show up in the
        // custom map at all, since it isn't claiming to define a new
        // filetype.
        let overrides = HashMap::from([(
            "rust".to_owned(),
            ServerOverride {
                command: "/opt/bin/rust-analyzer".to_owned(),
                ..ServerOverride::default()
            },
        )]);
        assert!(custom_extension_map(&overrides).is_empty());
    }

    #[test]
    fn custom_extension_map_drops_a_claim_on_a_built_in_extension() {
        let overrides = HashMap::from([(
            "not-rust-analyzer".to_owned(),
            ServerOverride {
                command: "echo".to_owned(),
                extensions: vec!["rs".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        // `.rs` already belongs to `Language::Rust` — the custom claim on
        // it never makes it into the map at all.
        assert!(!custom_extension_map(&overrides).contains_key("rs"));
    }

    #[test]
    fn custom_extension_map_picks_the_lexicographically_smallest_id_on_collision() {
        let overrides = HashMap::from([
            (
                "zsolargraph".to_owned(),
                ServerOverride {
                    command: "zsolargraph".to_owned(),
                    extensions: vec!["rb".to_owned()],
                    ..ServerOverride::default()
                },
            ),
            (
                "aruby".to_owned(),
                ServerOverride {
                    command: "aruby".to_owned(),
                    extensions: vec!["rb".to_owned()],
                    ..ServerOverride::default()
                },
            ),
        ]);
        // Deterministic regardless of the two ids' insertion/iteration
        // order: "aruby" sorts before "zsolargraph".
        assert_eq!(
            custom_extension_map(&overrides)
                .get("rb")
                .map(String::as_str),
            Some("aruby")
        );
    }

    #[test]
    fn custom_extension_map_normalizes_a_leading_dot_and_whitespace() {
        // A very plausible mistake — other tools' config formats (VSCode's
        // `files.associations`) do use a leading dot — that must not
        // silently make the entry permanently unroutable.
        let overrides = HashMap::from([(
            "ruby".to_owned(),
            ServerOverride {
                command: "solargraph".to_owned(),
                extensions: vec![".rb".to_owned(), " erb ".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        let map = custom_extension_map(&overrides);
        assert_eq!(map.get("rb").map(String::as_str), Some("ruby"));
        assert_eq!(map.get("erb").map(String::as_str), Some("ruby"));
        // And a real file actually routes through `LangKey::detect`, the
        // end-to-end path a leading-dot typo used to defeat silently.
        assert_eq!(
            LangKey::detect(Path::new("app.rb"), &map),
            Some(LangKey::Custom("ruby".to_owned()))
        );
    }

    #[test]
    fn custom_extension_map_skips_a_built_in_named_id() {
        // `rust` is a valid table name a user might reuse while
        // experimenting with an unrelated filetype — `<id>` decides
        // override-vs-custom (see `ServerOverride`'s docs), so a
        // built-in-named id must never also define a custom claim, even
        // though `.frag` itself isn't built-in-owned.
        let overrides = HashMap::from([(
            "rust".to_owned(),
            ServerOverride {
                command: "/opt/bin/rust-analyzer".to_owned(),
                extensions: vec!["frag".to_owned()],
                ..ServerOverride::default()
            },
        )]);
        assert!(custom_extension_map(&overrides).is_empty());
    }

    #[test]
    fn is_builtin_language_id_matches_all_six_lsp_ids_and_nothing_else() {
        for language in [
            Language::Rust,
            Language::TypeScript,
            Language::Python,
            Language::Go,
            Language::Kotlin,
            Language::Java,
        ] {
            assert!(is_builtin_language_id(language.lsp_id()));
        }
        assert!(!is_builtin_language_id("ruby"));
    }

    #[test]
    fn resolve_custom_server_reports_a_hint_when_command_is_empty() {
        let over = ServerOverride::default();
        let Err(err) = resolve_custom_server("ruby", &over) else {
            panic!("expected an empty command to be Unavailable");
        };
        assert!(err.reason.contains("ruby"));
        assert!(err.reason.contains("no command"));
        assert!(!err.installable, "a custom server is never auto-installed");
    }

    #[test]
    fn resolve_custom_server_builds_the_command_with_args_and_init_options() {
        let over = ServerOverride {
            command: "solargraph".to_owned(),
            args: vec!["stdio".to_owned()],
            initialization_options: Some(toml::Value::Boolean(true)),
            ..ServerOverride::default()
        };
        let resolved = resolve_custom_server("ruby", &over).unwrap();
        assert_eq!(resolved.command.get_program(), "solargraph");
        assert_eq!(
            resolved.command.get_args().collect::<Vec<_>>(),
            vec!["stdio"]
        );
        assert_eq!(
            resolved.initialization_options,
            Some(serde_json::json!(true))
        );
    }

    #[test]
    fn custom_workspace_root_falls_back_to_git_root_when_markers_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let file = repo_root.join("app").join("main.rb");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "").unwrap();

        assert_eq!(
            custom_workspace_root(&file, repo_root, &[]),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn custom_workspace_root_finds_the_nearest_configured_marker() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let project_dir = repo_root.join("services").join("api");
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(project_dir.join("Gemfile"), "").unwrap();
        let file = project_dir.join("app.rb");
        std::fs::write(&file, "").unwrap();

        let markers = vec!["Gemfile".to_owned()];
        assert_eq!(
            custom_workspace_root(&file, repo_root, &markers),
            project_dir
        );
    }

    #[test]
    fn custom_workspace_root_falls_back_to_git_root_when_no_marker_matches_in_range() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        let file = repo_root.join("app.rb");
        std::fs::write(&file, "").unwrap();

        let markers = vec!["Gemfile".to_owned()];
        assert_eq!(
            custom_workspace_root(&file, repo_root, &markers),
            repo_root.to_path_buf()
        );
    }

    #[test]
    fn toml_value_to_json_converts_every_scalar_and_container_variant() {
        assert_eq!(
            toml_value_to_json(&toml::Value::String("x".to_owned())),
            serde_json::json!("x")
        );
        assert_eq!(
            toml_value_to_json(&toml::Value::Integer(42)),
            serde_json::json!(42)
        );
        assert_eq!(
            toml_value_to_json(&toml::Value::Float(1.5)),
            serde_json::json!(1.5)
        );
        assert_eq!(
            toml_value_to_json(&toml::Value::Boolean(true)),
            serde_json::json!(true)
        );
        assert_eq!(
            toml_value_to_json(&toml::Value::Array(vec![
                toml::Value::Integer(1),
                toml::Value::Integer(2),
            ])),
            serde_json::json!([1, 2])
        );
        let table = toml::toml! {
            a = 1
            b = "two"
        };
        assert_eq!(
            toml_value_to_json(&toml::Value::Table(table)),
            serde_json::json!({"a": 1, "b": "two"})
        );
    }

    #[test]
    fn toml_value_to_json_stringifies_datetime() {
        let dt: toml::value::Datetime = "2026-08-07T00:00:00Z".parse().unwrap();
        let json = toml_value_to_json(&toml::Value::Datetime(dt));
        assert_eq!(json, serde_json::json!("2026-08-07T00:00:00Z"));
    }

    #[test]
    fn toml_value_to_json_maps_non_finite_floats_to_null() {
        // JSON's number grammar has no representation for NaN/infinity;
        // `serde_json::Number::from_f64` already returns `None` for those,
        // which this function surfaces as `Null` rather than panicking or
        // adding a `Result` its one (config-loading) caller would just
        // have to unwrap anyway — see this function's docs.
        assert_eq!(
            toml_value_to_json(&toml::Value::Float(f64::NAN)),
            serde_json::Value::Null
        );
        assert_eq!(
            toml_value_to_json(&toml::Value::Float(f64::INFINITY)),
            serde_json::Value::Null
        );
        assert_eq!(
            toml_value_to_json(&toml::Value::Float(f64::NEG_INFINITY)),
            serde_json::Value::Null
        );
    }
}
