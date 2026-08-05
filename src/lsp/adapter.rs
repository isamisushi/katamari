//! Which language server to launch for a given file, and where its
//! workspace root is. [`crate::lsp::manager::LspManager`] is the only
//! caller — everything it needs to know about *how* to start a server for a
//! language lives here, behind [`resolve_server`], so adding a second
//! language in a later milestone means adding one more match arm, not
//! touching the manager's spawn/queue/state-machine logic.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A language `LspManager` knows how to find a server for. M3a supports
/// exactly one; [`Language::detect`] returning `None` for every other
/// extension is what makes "no LSP support for this file type" a normal,
/// silent outcome rather than a special case each call site has to guard
/// against separately.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    Rust,
}

impl Language {
    pub fn detect(path: &Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("rs") => Some(Language::Rust),
            _ => None,
        }
    }

    /// The `languageId` sent in `textDocument/didOpen` — part of the LSP
    /// spec's vocabulary, not this codebase's; servers key syntax/feature
    /// behavior off it.
    pub fn lsp_id(self) -> &'static str {
        match self {
            Language::Rust => "rust",
        }
    }
}

/// Why no server could be started, in a form suitable for a status-bar
/// message — this is what a user sees when hovering doesn't work, so it
/// says what to do about it, not just what went wrong.
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
/// stays testable as pure path resolution.
pub fn resolve_server(language: Language) -> Result<Command, Unavailable> {
    match language {
        Language::Rust => resolve_rust_analyzer(),
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
    Err(Unavailable {
        reason: "rust-analyzer not found on PATH or via `rustup which rust-analyzer` — install it with `rustup component add rust-analyzer`".to_owned(),
    })
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

/// Walks up from `file`'s directory to the nearest ancestor containing a
/// `Cargo.toml`, refusing to search past `git_root` — a workspace root for
/// a file under review is never outside the repository that file belongs
/// to, even if some unrelated Cargo project happens to sit further up the
/// real filesystem tree.
pub fn workspace_root(file: &Path, git_root: &Path) -> Option<PathBuf> {
    let mut dir = file.parent()?;
    loop {
        if dir.join("Cargo.toml").is_file() {
            return Some(dir.to_path_buf());
        }
        if dir == git_root {
            return None;
        }
        dir = dir.parent()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust_by_extension_and_nothing_else() {
        assert_eq!(
            Language::detect(Path::new("src/main.rs")),
            Some(Language::Rust)
        );
        assert_eq!(Language::detect(Path::new("README.md")), None);
        assert_eq!(Language::detect(Path::new("no_extension")), None);
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

        assert_eq!(workspace_root(&file, repo_root), Some(crate_dir));
    }

    #[test]
    fn workspace_root_returns_none_when_no_cargo_toml_before_git_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path();
        std::fs::create_dir_all(repo_root.join("notes")).unwrap();
        let file = repo_root.join("notes").join("todo.rs");
        std::fs::write(&file, "").unwrap();

        assert_eq!(workspace_root(&file, repo_root), None);
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
            workspace_root(&file, repo_root),
            Some(repo_root.to_path_buf())
        );
    }
}
