//! Which language server to launch for a given file, and where its
//! workspace root is. [`crate::lsp::manager::LspManager`] is the only
//! caller — everything it needs to know about *how* to start a server for a
//! language lives here, behind [`resolve_server`], so adding a language
//! means adding one more match arm, not touching the manager's
//! spawn/queue/state-machine logic.

use crate::config::ServerOverride;
use std::collections::HashMap;
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

/// Why no server could be started, in a form suitable for a status-bar
/// message — this is what a user sees when a language feature doesn't work,
/// so it says what to do about it, not just what went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable {
    pub reason: String,
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.reason)
    }
}

/// Builds the command to launch `language`'s server. Never runs it —
/// spawning is [`crate::lsp::transport::Transport::spawn`]'s job — so this
/// stays testable as pure path resolution. `overrides` (config's
/// `[lsp.servers.<lang>]`, keyed by [`Language::lsp_id`]) takes priority
/// over every built-in lookup below when it has an entry for `language`,
/// exactly as a user's explicit config should — a pinned server version, a
/// wrapper script, or a server this module has no built-in support for at
/// all.
pub fn resolve_server(
    language: Language,
    workspace_root: &Path,
    overrides: &HashMap<String, ServerOverride>,
) -> Result<Command, Unavailable> {
    if let Some(over) = overrides.get(language.lsp_id()) {
        let mut command = Command::new(&over.command);
        command.args(&over.args);
        return Ok(command);
    }
    match language {
        Language::Rust => resolve_rust_analyzer(),
        Language::TypeScript => resolve_typescript_language_server(workspace_root),
        Language::Python => resolve_pyright(workspace_root),
        Language::Go => resolve_gopls(),
    }
}

/// Tries `rust-analyzer` on `PATH` first (the common case: it's a rustup
/// component and rustup's shims put it on `PATH` directly), then falls back
/// to `rustup which rust-analyzer` — the form that works when katamari
/// itself is invoked through `mise exec --`, whose sandboxed `PATH` doesn't
/// always include rustup's shim directory even though `rustup` itself is
/// reachable.
fn resolve_rust_analyzer() -> Result<Command, Unavailable> {
    if let Some(path) = which_on_path("rust-analyzer") {
        return Ok(Command::new(path));
    }
    if let Some(path) = rustup_which("rust-analyzer") {
        return Ok(Command::new(path));
    }
    Err(unavailable(
        "rust",
        "rust-analyzer not found on PATH or via `rustup which rust-analyzer` — install it with `rustup component add rust-analyzer`",
    ))
}

/// Tries the project-local install first (`node_modules/.bin/...`, the
/// common case for a JS/TS project that lists it as a devDependency — a
/// project-pinned server version is more likely to agree with the project's
/// own `tsconfig.json` than whatever happens to be globally installed),
/// then falls back to `PATH`.
fn resolve_typescript_language_server(workspace_root: &Path) -> Result<Command, Unavailable> {
    let local = workspace_root
        .join("node_modules")
        .join(".bin")
        .join("typescript-language-server");
    if local.is_file() {
        return Ok(ts_language_server_command(local));
    }
    if let Some(path) = which_on_path("typescript-language-server") {
        return Ok(ts_language_server_command(path));
    }
    Err(unavailable(
        "typescript",
        "typescript-language-server not found — install it with `npm i -g typescript-language-server typescript`",
    ))
}

fn ts_language_server_command(path: PathBuf) -> Command {
    let mut command = Command::new(path);
    command.arg("--stdio");
    command
}

/// Tries the project-local virtualenv first (`.venv/bin/pyright-langserver`
/// — the common convention for a Python project's own interpreter and
/// dependencies), then falls back to `PATH`.
fn resolve_pyright(workspace_root: &Path) -> Result<Command, Unavailable> {
    let local = workspace_root
        .join(".venv")
        .join("bin")
        .join("pyright-langserver");
    if local.is_file() {
        return Ok(pyright_command(local));
    }
    if let Some(path) = which_on_path("pyright-langserver") {
        return Ok(pyright_command(path));
    }
    Err(unavailable(
        "python",
        "pyright-langserver not found — install it with `npm i -g pyright`",
    ))
}

fn pyright_command(path: PathBuf) -> Command {
    let mut command = Command::new(path);
    command.arg("--stdio");
    command
}

/// `gopls` has no meaningful project-local install location (it's a single
/// Go-toolchain binary, not a per-project dependency the way node/python
/// servers are) — `PATH` is the only place to look.
fn resolve_gopls() -> Result<Command, Unavailable> {
    if let Some(path) = which_on_path("gopls") {
        return Ok(Command::new(path));
    }
    Err(unavailable(
        "go",
        "gopls not found on PATH — install it with `go install golang.org/x/tools/gopls@latest`",
    ))
}

fn unavailable(language_name: &str, hint: &str) -> Unavailable {
    Unavailable {
        reason: format!("LSP: {language_name} \u{2715} \u{2014} {hint}"),
    }
}

fn which_on_path(bin: &str) -> Option<PathBuf> {
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
    let markers = root_markers(language);
    let mut dir = start;
    loop {
        if markers.iter().any(|marker| dir.join(marker).is_file()) {
            return Some(dir.to_path_buf());
        }
        if dir == git_root {
            return None;
        }
        match dir.parent() {
            Some(parent) => dir = parent,
            None => return None,
        }
    }
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
    }
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
        assert_eq!(Language::detect(Path::new("README.md")), None);
        assert_eq!(Language::detect(Path::new("no_extension")), None);
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
        if let Err(unavailable) = resolve_gopls() {
            assert!(unavailable.reason.contains("gopls"));
            assert!(unavailable.reason.starts_with("LSP: go"));
        }
    }

    #[test]
    fn a_config_override_takes_priority_over_the_built_in_lookup() {
        let overrides = HashMap::from([(
            "go".to_owned(),
            ServerOverride {
                command: "/opt/bin/my-gopls".to_owned(),
                args: vec!["--stdio".to_owned()],
            },
        )]);
        let command = resolve_server(Language::Go, Path::new("/repo"), &overrides).unwrap();
        assert_eq!(command.get_program(), "/opt/bin/my-gopls");
        assert_eq!(command.get_args().collect::<Vec<_>>(), vec!["--stdio"]);
    }

    #[test]
    fn no_override_falls_through_to_the_built_in_lookup() {
        // No "go" entry in `overrides` — resolution falls through to
        // `resolve_gopls`, matching `resolve_server_reports_a_hint_when_a_binary_is_missing`'s
        // own caveat about this workspace's PATH.
        let overrides = HashMap::new();
        let result = resolve_server(Language::Go, Path::new("/repo"), &overrides);
        if let Err(unavailable) = result {
            assert!(unavailable.reason.starts_with("LSP: go"));
        }
    }
}
