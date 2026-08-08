//! Downloads and installs a language server into a katamari-owned prefix
//! when [`crate::lsp::adapter::resolve_server`] can't find one anywhere
//! else — the "it just works" UX VSCode/Zed users expect, without
//! katamari embedding a package manager of its own. Only
//! [`crate::lsp::manager::LspManager`]'s spawn thread ever calls [`ensure`]:
//! that's the one place already expected to block on slow, network-bound
//! work (see that module's docs on why server startup happens off the
//! caller's thread), so this module does real network I/O without
//! apology — unlike [`crate::lsp::adapter`], which stays network-free so it
//! stays cheaply testable. `ktmr lsp install`/`ktmr lsp update` (see
//! `main.rs`) call it directly, for a user who wants to trigger or refresh
//! an install without waiting for a server to be needed.
//!
//! Every server lands at `<prefix>/<dir-name>/<pinned-version>/...`, one
//! subdirectory per pinned version, never mutated in place — a version
//! bump in the constants below just adds a new subdirectory rather than
//! overwriting a binary a running server might still have mapped into
//! memory, and a failed or partial install never leaves anything at the
//! *final* path for [`installed_binary_path`] to mistake for a working
//! install (see [`atomic_write_executable`] for the single-file case; the
//! npm and `go install` strategies get the same property for free, since
//! neither one ever populates `node_modules/.bin`/`$GOBIN` without
//! succeeding first).
//!
//! HTTPS to five kinds of host only, enforced by [`assert_allowed_host`]:
//! `github.com` (rust-analyzer releases — note the initial request's
//! redirect to a signed, time-limited `githubusercontent.com` asset URL is
//! trusted implicitly, since it's `github.com` itself issuing that
//! redirect, not a third party), `nodejs.org` (the Node.js bootstrap),
//! `download-cdn.jetbrains.com` (kotlin-lsp releases — JetBrains' own CDN,
//! not fronted through GitHub release assets the way rust-analyzer's are),
//! `download.eclipse.org` (jdtls milestone releases), and the npm registry
//! (reached through `npm` itself, which this module treats as a trusted
//! subprocess rather than an HTTP endpoint it talks to directly).

use crate::lsp::adapter::{Language, jdtls_config_dir_name, mise_which, which_on_path};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::process::Command;

// --- Pinned versions -------------------------------------------------------
//
// Verified against each project's real release feed at the time these were
// pinned (see the M8b commit message for the exact commands run). Bumping
// one of these is the entirety of what `ktmr lsp update` needs to pick up —
// no other code in this module names a version number directly.

const RUST_ANALYZER_VERSION: &str = "2026-08-03";
const TS_LANGUAGE_SERVER_VERSION: &str = "5.3.0";
/// Deliberately *not* npm's `latest` (7.x at the time this was pinned):
/// TypeScript 7 is Microsoft's native-compiler rewrite, whose package no
/// longer ships `lib/tsserver.js` at all (see
/// `install::managed_tsserver_path`'s docs) — installing it here would
/// silently break every managed typescript-language-server, which still
/// speaks the classic JS-based tsserver protocol. `6.0.3` is the newest
/// stable release before that rewrite.
const TYPESCRIPT_VERSION: &str = "6.0.3";
const PYRIGHT_VERSION: &str = "1.1.411";
const GOPLS_VERSION: &str = "0.23.0";
/// JetBrains' own version scheme (`<build>.<build-number>.<patch>`, not
/// semver) — matches both the CDN path segment and every asset filename for
/// a given release, e.g. `kotlin-server-262.9593.0-aarch64.sit`. The GitHub
/// release tag (`kotlin-lsp/v262.9593.0`) carries the same number but isn't
/// used here: releases are hosted on JetBrains' CDN, not as GitHub release
/// assets — see [`install_kotlin_lsp`].
const KOTLIN_LSP_VERSION: &str = "262.9593.0";
/// Node.js LTS used only as a bootstrap runtime for the two npm-based
/// strategies below, when no `npm` can be found anywhere (not even via
/// `mise which npm`) — not itself one of the six languages katamari
/// reviews.
const NODE_BOOTSTRAP_VERSION: &str = "24.19.0";
/// jdtls 1.60.0. Eclipse's milestone releases embed a build timestamp in
/// both the tarball filename and (independently) the launcher jar's
/// filename inside it — neither is derivable from `JDTLS_VERSION` alone, so
/// a version bump must update all three of these constants together. To
/// find the new values for a bump: `latest.txt` in the milestone's own
/// directory (`https://download.eclipse.org/jdtls/milestones/<ver>/latest.txt`)
/// resolves the exact tarball filename, and the launcher jar's versioned
/// name is only discoverable by listing `plugins/` inside that tarball —
/// there's no separate feed for it. [`install_jdtls`] fails loudly rather
/// than silently if [`JDTLS_LAUNCHER_JAR`]'s pin has gone stale for a given
/// [`JDTLS_TARBALL`] (the jar it expects to find after extracting simply
/// won't be there under a mismatched pin).
const JDTLS_VERSION: &str = "1.60.0";
const JDTLS_TARBALL: &str = "jdt-language-server-1.60.0-202606262232.tar.gz";
/// Referenced by its exact filename from `config_<os>/config.ini`'s
/// `osgi.bundles` line inside the real jdtls tarball — this is why the jar
/// is never renamed on install (see [`install_jdtls`]) and why its filename
/// is pinned as its own constant rather than derived from
/// [`JDTLS_VERSION`]. `pub(crate)` so [`crate::lsp::adapter`]'s
/// managed-launch command building can reference the exact same name
/// rather than duplicating the pin.
pub(crate) const JDTLS_LAUNCHER_JAR: &str =
    "org.eclipse.equinox.launcher_1.7.200.v20260619-2039.jar";

/// One pinned server's on-disk identity: the directory name under
/// `<prefix>/` and the pinned version that names its version subdirectory.
/// Both [`installed_binary_path`] (read-only, called from
/// [`crate::lsp::adapter`]) and this module's own installers key off the
/// same [`spec_for`] lookup, so the two can never disagree about where a
/// given language's binary lives.
#[derive(Debug, Clone, Copy)]
struct ServerSpec {
    dir_name: &'static str,
    version: &'static str,
}

/// Every language's on-disk identity, matched exhaustively rather than
/// looked up in a `REGISTRY` array — the compiler forces every
/// [`Language`] variant to have an arm here at compile time, instead of a
/// `.find(...).expect(...)` panicking at runtime the day a variant is added
/// without a matching entry.
fn spec_for(language: Language) -> ServerSpec {
    match language {
        Language::Rust => ServerSpec {
            dir_name: "rust-analyzer",
            version: RUST_ANALYZER_VERSION,
        },
        Language::TypeScript => ServerSpec {
            dir_name: "typescript-language-server",
            version: TS_LANGUAGE_SERVER_VERSION,
        },
        Language::Python => ServerSpec {
            dir_name: "pyright",
            version: PYRIGHT_VERSION,
        },
        Language::Go => ServerSpec {
            dir_name: "gopls",
            version: GOPLS_VERSION,
        },
        Language::Kotlin => ServerSpec {
            dir_name: "kotlin-lsp",
            version: KOTLIN_LSP_VERSION,
        },
        Language::Java => ServerSpec {
            dir_name: "jdt-language-server",
            version: JDTLS_VERSION,
        },
    }
}

/// Where the pinned binary for `language` would live under `prefix` once
/// installed — regardless of whether it actually exists yet; see
/// [`installed_binary_path`] for the version that checks.
fn binary_path(prefix: &Path, language: Language) -> PathBuf {
    let spec = spec_for(language);
    let version_dir = prefix.join(spec.dir_name).join(spec.version);
    match language {
        Language::Rust => version_dir.join("rust-analyzer"),
        Language::TypeScript => version_dir
            .join("node_modules")
            .join(".bin")
            .join("typescript-language-server"),
        Language::Python => version_dir
            .join("node_modules")
            .join(".bin")
            .join("pyright-langserver"),
        Language::Go => version_dir.join("gopls"),
        // The archive's sole top-level directory
        // (`kotlin-server-<version>[-aarch64]/`) is renamed to `version_dir`
        // itself on install (mirroring `bootstrap_node`'s approach — see
        // `install_kotlin_lsp`), so the launcher always lands at the same
        // path regardless of which per-arch asset produced it.
        Language::Kotlin => version_dir.join("bin").join("intellij-server"),
        // Unlike Kotlin, jdtls's tarball has no wrapping top-level
        // directory to rename away (see `install_jdtls`) — `version_dir`
        // *is* the extracted tree, so this is a pure join with no per-arch
        // variation to account for (jdtls ships one platform-independent
        // tarball).
        Language::Java => version_dir.join("plugins").join(JDTLS_LAUNCHER_JAR),
    }
}

/// The pinned TypeScript compiler's `tsserver.js`, installed as a peer
/// dependency alongside typescript-language-server in the very same npm
/// `--prefix` (see [`install_npm_package`]'s `peers` argument). Needed
/// because typescript-language-server does *not* look here on its own: its
/// default module resolution for `typescript` walks up from the workspace
/// being edited, which katamari's managed install directory is never part
/// of — so a project with no `typescript` devDependency of its own would
/// fail to initialize entirely without this. `adapter::command_for` passes
/// it explicitly via `--tsserver-path` whenever the server itself resolved
/// from this same managed install (a project-local or `PATH` server is left
/// to its own default resolution, which is more likely to already agree
/// with that project's own `tsconfig.json`/typescript version).
/// Where [`bootstrap_node`]'s runtime lands — the directory
/// [`crate::lsp::adapter`] must put on a managed npm-strategy server's
/// child `PATH`, since those servers are `#!/usr/bin/env node` scripts and
/// the bootstrap is only ever taken on machines where no other `node`
/// exists to satisfy that shebang.
pub(crate) fn bootstrapped_node_bin_dir() -> PathBuf {
    prefix_dir()
        .join("node")
        .join(NODE_BOOTSTRAP_VERSION)
        .join("bin")
}

pub fn managed_tsserver_path() -> PathBuf {
    let spec = spec_for(Language::TypeScript);
    prefix_dir()
        .join(spec.dir_name)
        .join(spec.version)
        .join("node_modules")
        .join("typescript")
        .join("lib")
        .join("tsserver.js")
}

/// `<prefix>/<dir-name>/<pinned-version>/...`'s binary for `language`, if
/// it's already there and executable — the idempotency check every install
/// strategy and [`crate::lsp::adapter`]'s katamari-managed lookup tier both
/// go through, so "already installed" always means the exact same thing.
/// Purely a filesystem check: no network, safe to call from `adapter`.
pub fn installed_binary_path(prefix: &Path, language: Language) -> Option<PathBuf> {
    let path = binary_path(prefix, language);
    is_executable(&path).then_some(path)
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// `~/.local/share/katamari/servers` (or `$XDG_DATA_HOME/katamari/servers`
/// when set) — every managed server install's root. A thin wrapper over
/// [`prefix_dir_from_env`] reading the real process environment; production
/// code calls this, tests call the pure function directly with fabricated
/// values so they don't need to mutate process-wide env vars other tests
/// might be reading concurrently.
pub fn prefix_dir() -> PathBuf {
    prefix_dir_from_env(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"))
}

fn prefix_dir_from_env(
    xdg_data_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    let base = match xdg_data_home {
        Some(xdg) if !xdg.is_empty() => PathBuf::from(xdg),
        _ => PathBuf::from(home.unwrap_or_else(|| std::ffi::OsString::from(".")))
            .join(".local")
            .join("share"),
    };
    base.join("katamari").join("servers")
}

/// Why an install attempt failed — `Display`s as a message suitable for
/// [`crate::lsp::manager::ServerState::Unavailable`]'s reason, the same way
/// [`crate::lsp::adapter::Unavailable`] does for a resolution failure.
#[derive(Debug)]
pub struct InstallError(String);

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for InstallError {}

/// Installs `language`'s pinned server into [`prefix_dir`], or returns the
/// path to it immediately (no network) if it's already there — see this
/// module's doc comment for the on-disk layout and idempotency guarantee.
/// `on_progress` is called zero or more times with a human-readable status
/// line as the install proceeds (download started, extracting, running
/// `npm install`, ...); [`crate::lsp::manager::LspManager::spawn_server`] is
/// the production caller, forwarding each call into
/// `ServerState::Installing` for the status bar, but `ktmr lsp install`
/// (see `main.rs`) just prints each one.
pub fn ensure(
    language: Language,
    on_progress: impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    ensure_in(&prefix_dir(), language, on_progress)
}

fn ensure_in(
    prefix: &Path,
    language: Language,
    mut on_progress: impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    if let Some(path) = installed_binary_path(prefix, language) {
        return Ok(path);
    }
    match language {
        Language::Rust => install_rust_analyzer(prefix, &mut on_progress),
        Language::TypeScript => install_npm_package(
            prefix,
            Language::TypeScript,
            "typescript-language-server",
            &[("typescript", TYPESCRIPT_VERSION)],
            "typescript-language-server",
            &mut on_progress,
        ),
        Language::Python => install_npm_package(
            prefix,
            Language::Python,
            "pyright",
            &[],
            "pyright-langserver",
            &mut on_progress,
        ),
        Language::Go => install_gopls(prefix, &mut on_progress),
        Language::Kotlin => install_kotlin_lsp(prefix, &mut on_progress),
        // Deliberately no JDK probe here — see `install_jdtls`'s docs: an
        // install can proceed with nothing but a download, so `ktmr lsp
        // install java` succeeding while `ktmr lsp doctor` still reports
        // "needs a JDK" is correct, self-consistent behavior, not a bug.
        // The JDK gate lives entirely in `adapter`'s diagnose/command path.
        Language::Java => install_jdtls(prefix, &mut on_progress),
    }
}

// --- rust-analyzer: prebuilt binary from GitHub releases --------------------

/// Maps a target to the exact asset filename rust-analyzer's release
/// publishes for it — pure string logic, split out from
/// [`install_rust_analyzer`] so the target-triple mapping is unit-testable
/// without a network call. Unsupported combinations (anything but
/// aarch64/x86_64 macOS/Linux) get a clear message rather than a
/// downstream 404, since rust-analyzer simply doesn't publish a prebuilt
/// binary for them.
fn rust_analyzer_asset_name(os: &str, arch: &str) -> Result<String, String> {
    let triple = match (os, arch) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        _ => {
            return Err(format!(
                "katamari has no prebuilt rust-analyzer for {os}/{arch} — install it manually \
                 (see https://rust-analyzer.github.io/manual.html#installation)"
            ));
        }
    };
    Ok(format!("rust-analyzer-{triple}.gz"))
}

/// Downloads, gunzips, and installs the pinned rust-analyzer build for the
/// current platform. The release asset is a single gzip-compressed binary
/// (not a tarball) — decompressed entirely in memory, since rust-analyzer
/// binaries are tens of megabytes, not large enough to need streaming
/// decompression to disk.
fn install_rust_analyzer(
    prefix: &Path,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    let asset = rust_analyzer_asset_name(std::env::consts::OS, std::env::consts::ARCH)
        .map_err(InstallError)?;
    let spec = spec_for(Language::Rust);
    let version_dir = prefix.join(spec.dir_name).join(spec.version);
    create_dir_all(&version_dir)?;

    let url = format!(
        "https://github.com/rust-lang/rust-analyzer/releases/download/{}/{asset}",
        spec.version
    );
    on_progress(format!("downloading rust-analyzer {}", spec.version));
    let compressed = http_get(&url)?;

    on_progress("extracting rust-analyzer".to_owned());
    let mut decoder = flate2::read::GzDecoder::new(&compressed[..]);
    let mut binary = Vec::new();
    decoder
        .read_to_end(&mut binary)
        .map_err(|e| InstallError(format!("gunzip: {e}")))?;

    let final_path = version_dir.join("rust-analyzer");
    atomic_write_executable(&final_path, &binary)?;
    Ok(final_path)
}

// --- typescript-language-server / pyright: npm ------------------------------

/// `npm install --prefix <install-dir> <npm_package>@<version> [peer@ver
/// ...]`, then locates the resulting binary at
/// `<install-dir>/node_modules/.bin/<bin_name>` — the npm convention for
/// where a package's declared `bin` entries land. Not literally atomic the
/// way [`atomic_write_executable`] is (there's no single file to rename
/// into place), but equivalently safe: [`installed_binary_path`] only ever
/// considers the *result* at that exact `.bin` path, and npm never
/// populates it until the install it's part of has fully succeeded, so a
/// failed or interrupted `npm install` simply leaves nothing there for a
/// later [`ensure`] call to mistake for a working install — it just retries
/// from scratch, same as a failed download would.
fn install_npm_package(
    prefix: &Path,
    language: Language,
    npm_package: &str,
    peers: &[(&str, &str)],
    bin_name: &str,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    let npm = resolve_npm(prefix, on_progress)?;
    let spec = spec_for(language);
    let install_dir = prefix.join(spec.dir_name).join(spec.version);
    create_dir_all(&install_dir)?;

    let mut package_specs = vec![format!("{npm_package}@{}", spec.version)];
    package_specs.extend(peers.iter().map(|(pkg, ver)| format!("{pkg}@{ver}")));

    on_progress(format!("npm install {}", package_specs.join(" ")));
    let mut npm_command = Command::new(&npm);
    npm_command
        .arg("install")
        .arg("--no-audit")
        .arg("--no-fund")
        .arg("--prefix")
        .arg(&install_dir)
        .args(&package_specs);
    // `npm` is a `#!/usr/bin/env node` script, so the child must be able to
    // resolve plain `node` on its PATH — which a machine with no Node at all
    // (the whole reason `bootstrap_node` ran) by definition can't. The
    // bootstrap lays `node` out next to `npm` in the same `bin/`, so
    // prepending npm's own directory makes the shebang resolve regardless of
    // which tier `resolve_npm` found npm through; without this the install
    // dies with a bare exit-127 before npm even starts.
    if let Some(bin_dir) = npm.parent() {
        let mut paths = vec![bin_dir.to_path_buf()];
        if let Some(existing) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            npm_command.env("PATH", joined);
        }
    }
    let status = npm_command
        .status()
        .map_err(|e| InstallError(format!("running {}: {e}", npm.display())))?;
    if !status.success() {
        return Err(InstallError(format!(
            "npm install {} failed ({status})",
            package_specs.join(" ")
        )));
    }

    let binary = install_dir.join("node_modules").join(".bin").join(bin_name);
    if !is_executable(&binary) {
        return Err(InstallError(format!(
            "npm install succeeded but {} was not produced",
            binary.display()
        )));
    }
    Ok(binary)
}

/// `npm` itself, tried on `PATH` then via `mise which npm` — the same
/// two-tier probe [`crate::lsp::adapter`] uses for every server binary,
/// reused here since npm is just as likely to be mise-managed as any
/// language server is. Falls through to bootstrapping a private Node.js
/// runtime (see [`bootstrap_node`]) only when neither finds one — the
/// documented last resort, since a real system Node install is virtually
/// always preferable (already on `PATH` for the user's own tools, kept
/// up to date by whatever manages it) to katamari owning a redundant copy.
fn resolve_npm(
    prefix: &Path,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    if let Some(npm) = which_on_path("npm") {
        return Ok(npm);
    }
    if let Some(npm) = mise_which("npm") {
        return Ok(npm);
    }
    on_progress("no npm found on PATH or via `mise which npm`; bootstrapping Node.js".to_owned());
    bootstrap_node(prefix, on_progress)
}

// --- gopls: go install -------------------------------------------------------

/// `go install golang.org/x/tools/gopls@v<pinned>` with `GOBIN` pointed at
/// this server's version directory. Requires a go toolchain the same two
/// places every other lookup checks (`PATH`, `mise which go`) — deliberately
/// *not* bootstrapped the way Node.js is for the npm strategies: a
/// developer working on Go code already has a Go toolchain, so the absence
/// of one here is far more likely to mean "not a Go project" than "Go
/// developer with no Go installed," and installing an entire toolchain on
/// their behalf would be presumptuous in a way downloading a small
/// prebuilt binary or a Node runtime for two servers isn't. See
/// [`crate::lsp::adapter::diagnose`]'s `installable_if_missing` field, which
/// reflects exactly this: `false` when no go toolchain is reachable at all.
fn install_gopls(
    prefix: &Path,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    let go = which_on_path("go")
        .or_else(|| mise_which("go"))
        .ok_or_else(|| {
            InstallError(
                "no go toolchain found on PATH or via `mise which go` — install Go first \
             (https://go.dev/dl/), then retry"
                    .to_owned(),
            )
        })?;
    let spec = spec_for(Language::Go);
    let bin_dir = prefix.join(spec.dir_name).join(spec.version);
    create_dir_all(&bin_dir)?;

    on_progress(format!("go install gopls@v{}", spec.version));
    let status = Command::new(&go)
        .arg("install")
        .arg(format!("golang.org/x/tools/gopls@v{}", spec.version))
        .env("GOBIN", &bin_dir)
        .status()
        .map_err(|e| InstallError(format!("running {}: {e}", go.display())))?;
    if !status.success() {
        return Err(InstallError(format!(
            "go install gopls@v{} failed ({status})",
            spec.version
        )));
    }

    let binary = bin_dir.join("gopls");
    if !is_executable(&binary) {
        return Err(InstallError(format!(
            "go install succeeded but {} was not produced",
            binary.display()
        )));
    }
    Ok(binary)
}

// --- kotlin-lsp: self-contained archive from JetBrains' CDN -----------------

/// Maps a target to the exact asset filename kotlin-lsp's release publishes
/// for it — the same pure-mapping split [`rust_analyzer_asset_name`] gets,
/// for the same reason (unit-testable target logic, no network). macOS
/// assets are named `.sit` (a vestige of JetBrains' release tooling) but are
/// ordinary zip archives in practice — verified by hand against the real
/// download, not an assumption; see [`install_kotlin_lsp`]'s docs. Linux
/// assets are real `.tar.gz`. Windows isn't a supported katamari platform at
/// all yet, so it isn't listed here despite kotlin-lsp itself publishing for
/// it.
fn kotlin_lsp_asset_name(os: &str, arch: &str) -> Result<String, String> {
    let version = KOTLIN_LSP_VERSION;
    let name = match (os, arch) {
        ("macos", "aarch64") => format!("kotlin-server-{version}-aarch64.sit"),
        ("macos", "x86_64") => format!("kotlin-server-{version}.sit"),
        ("linux", "aarch64") => format!("kotlin-server-{version}-aarch64.tar.gz"),
        ("linux", "x86_64") => format!("kotlin-server-{version}.tar.gz"),
        _ => {
            return Err(format!(
                "katamari has no prebuilt kotlin-lsp for {os}/{arch} — install it manually \
                 (see https://github.com/Kotlin/kotlin-lsp)"
            ));
        }
    };
    Ok(name)
}

/// Downloads and installs the pinned kotlin-lsp build for the current
/// platform — JetBrains' "Standalone Kotlin LSP Archive", not the VS
/// Code-only `.vsix` build. Ships its own JetBrains Runtime (JBR): the
/// archive's `bin/intellij-server` is a native launcher that finds its
/// bundled `jbr/` alongside itself, so — unlike gopls, which needs an
/// external Go toolchain to `go install` against — this needs no JVM probe
/// and no external Java at all, on this machine or the end user's; verified
/// by extracting a real release and finding a complete JBR under `jbr/`,
/// and by running the extracted `bin/intellij-server --help` standalone.
///
/// Despite the `.sit` extension (a StuffIt-archive holdover in JetBrains'
/// own naming, not a StuffIt archive at all), the macOS assets are ordinary
/// zip files — confirmed by downloading one and inspecting it with `file`
/// before writing this — so [`extract_archive`] dispatches purely on
/// filename suffix (`.sit`/`.zip` vs `.tar.gz`) rather than sniffing
/// content. The archive's sole top-level entry
/// (`kotlin-server-<version>[-<arch>]/`) is renamed into place exactly the
/// way [`bootstrap_node`] handles Node's tarball, so [`binary_path`] doesn't
/// need to know that per-asset directory name.
fn install_kotlin_lsp(
    prefix: &Path,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    let asset = kotlin_lsp_asset_name(std::env::consts::OS, std::env::consts::ARCH)
        .map_err(InstallError)?;
    let spec = spec_for(Language::Kotlin);
    let version_dir = prefix.join(spec.dir_name).join(spec.version);
    let parent_dir = prefix.join(spec.dir_name);
    create_dir_all(&parent_dir)?;

    let url = format!(
        "https://download-cdn.jetbrains.com/language-server/kotlin-server/{}/{asset}",
        spec.version
    );
    on_progress(format!("downloading kotlin-lsp {}", spec.version));
    let archive = http_get(&url)?;

    on_progress("extracting kotlin-lsp".to_owned());
    let tmp_dir = parent_dir.join(format!(".tmp-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    create_dir_all(&tmp_dir)?;
    extract_archive(&asset, &archive, &tmp_dir)?;

    // The archive's sole top-level entry is `kotlin-server-<version>[-<arch>]/`.
    let extracted = std::fs::read_dir(&tmp_dir)
        .map_err(|e| InstallError(format!("reading {}: {e}", tmp_dir.display())))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|p| p.is_dir())
        .ok_or_else(|| InstallError("kotlin-lsp archive had no top-level directory".to_owned()))?;
    let _ = std::fs::remove_dir_all(&version_dir);
    std::fs::rename(&extracted, &version_dir).map_err(|e| {
        InstallError(format!(
            "installing kotlin-lsp into {}: {e}",
            version_dir.display()
        ))
    })?;
    let _ = std::fs::remove_dir_all(&tmp_dir);

    let binary = version_dir.join("bin").join("intellij-server");
    ensure_executable(&binary)?;
    if !is_executable(&binary) {
        return Err(InstallError(format!(
            "kotlin-lsp archive extracted but {} was not produced",
            binary.display()
        )));
    }
    Ok(binary)
}

// --- jdtls: prebuilt tarball from Eclipse's milestone downloads -------------

/// Downloads and installs the pinned jdtls build — platform-independent
/// (Eclipse ships one tarball for every OS/arch, unlike rust-analyzer's or
/// kotlin-lsp's per-target assets, so there's no `<os>/<arch>` asset-name
/// mapping needed here). Verified by hand against the real 1.60.0 release:
/// the tarball extracts FLAT — `bin/`, `config_linux/`, `plugins/`, etc. sit
/// directly at its top level with no wrapping directory the way
/// kotlin-lsp's archive has — so unlike [`install_kotlin_lsp`]'s
/// find-the-sole-subdirectory approach, the temporary extraction directory
/// itself is renamed straight into `version_dir`.
fn install_jdtls(
    prefix: &Path,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    let spec = spec_for(Language::Java);
    let parent_dir = prefix.join(spec.dir_name);
    let version_dir = parent_dir.join(spec.version);
    create_dir_all(&parent_dir)?;

    let url = format!(
        "https://download.eclipse.org/jdtls/milestones/{}/{JDTLS_TARBALL}",
        spec.version
    );
    on_progress(format!("downloading jdt-language-server {}", spec.version));
    let archive = http_get(&url)?;

    on_progress("extracting jdt-language-server".to_owned());
    let tmp_dir = parent_dir.join(format!(".tmp-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    create_dir_all(&tmp_dir)?;
    extract_archive(JDTLS_TARBALL, &archive, &tmp_dir)?;

    let _ = std::fs::remove_dir_all(&version_dir);
    std::fs::rename(&tmp_dir, &version_dir).map_err(|e| {
        InstallError(format!(
            "installing jdt-language-server into {}: {e}",
            version_dir.display()
        ))
    })?;

    // The launcher jar is referenced by config.ini using this exact
    // versioned filename (see `JDTLS_LAUNCHER_JAR`'s docs), so its absence
    // here means the pin itself has gone stale for this tarball — a loud
    // failure now is far more useful than a server that spawns and then
    // fails to find its own `-jar` argument.
    let launcher = version_dir.join("plugins").join(JDTLS_LAUNCHER_JAR);
    ensure_executable(&launcher)?;
    if !is_executable(&launcher) {
        return Err(InstallError(format!(
            "jdt-language-server {} extracted, but its pinned launcher jar {} was not found — \
             JDTLS_LAUNCHER_JAR has likely gone stale for this version (see this module's \
             version-pin doc comment)",
            spec.version,
            launcher.display()
        )));
    }

    let config_dir_name = jdtls_config_dir_name(std::env::consts::OS).map_err(InstallError)?;
    let config_dir = version_dir.join(config_dir_name);
    if !config_dir.is_dir() {
        return Err(InstallError(format!(
            "jdt-language-server {} extracted, but its shared-configuration directory {} was not \
             found",
            spec.version,
            config_dir.display()
        )));
    }

    Ok(launcher)
}

/// Extracts `bytes` (the full body of `asset`) into `dest_dir`, dispatching
/// on `asset`'s filename suffix: `.tar.gz` the same gzip+tar path
/// [`bootstrap_node`] uses, `.sit`/`.zip` via the `zip` crate. Both are
/// in-memory sources (`Cursor`/[`flate2::read::GzDecoder`]) rather than a
/// temp file on disk first — neither release asset is large enough (tens to
/// a few hundred megabytes) to need streaming extraction.
fn extract_archive(asset: &str, bytes: &[u8], dest_dir: &Path) -> Result<(), InstallError> {
    if asset.ends_with(".tar.gz") {
        let decoder = flate2::read::GzDecoder::new(bytes);
        tar::Archive::new(decoder)
            .unpack(dest_dir)
            .map_err(|e| InstallError(format!("extracting {asset}: {e}")))
    } else {
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
            .map_err(|e| InstallError(format!("reading {asset} as a zip archive: {e}")))?;
        archive
            .extract(dest_dir)
            .map_err(|e| InstallError(format!("extracting {asset}: {e}")))
    }
}

/// Sets the executable bit on `path` if it isn't already set — a fallback
/// for [`extract_archive`]'s zip path, whose unix-permission preservation
/// depends on the archive actually storing them (it does, for the real
/// kotlin-lsp release, but this keeps `install_kotlin_lsp` correct even
/// against a differently-packaged future release rather than failing with
/// "extracted but not executable" for a reason a user can't fix). A no-op on
/// non-unix targets, matching [`is_executable`]'s own unix/non-unix split.
fn ensure_executable(path: &Path) -> Result<(), InstallError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            if perms.mode() & 0o111 == 0 {
                perms.set_mode(perms.mode() | 0o755);
                std::fs::set_permissions(path, perms)
                    .map_err(|e| InstallError(format!("chmod {}: {e}", path.display())))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

// --- Node.js bootstrap (last resort for the npm strategies) -----------------

/// Maps a target to Node's official tarball filename — the same
/// pure-mapping split [`rust_analyzer_asset_name`] gets, for the same
/// reason (unit-testable target logic, no network).
fn node_asset_name(os: &str, arch: &str) -> Result<String, String> {
    let (node_os, node_arch) = match (os, arch) {
        ("macos", "aarch64") => ("darwin", "arm64"),
        ("macos", "x86_64") => ("darwin", "x64"),
        ("linux", "aarch64") => ("linux", "arm64"),
        ("linux", "x86_64") => ("linux", "x64"),
        _ => {
            return Err(format!(
                "katamari has no Node.js bootstrap build for {os}/{arch}"
            ));
        }
    };
    Ok(format!(
        "node-v{NODE_BOOTSTRAP_VERSION}-{node_os}-{node_arch}.tar.gz"
    ))
}

/// Downloads and extracts the official Node.js LTS tarball into
/// `<prefix>/node/<version>/`, for [`resolve_npm`] to fall back to when no
/// system or mise-managed npm exists. Extraction lands in a temp directory
/// first, then a single `rename` moves the tarball's sole top-level
/// directory into place — atomic the same way
/// [`atomic_write_executable`] is for a single file, just at the directory
/// granularity extraction naturally produces here (no need to recursively
/// copy the tree ourselves).
fn bootstrap_node(
    prefix: &Path,
    on_progress: &mut impl FnMut(String),
) -> Result<PathBuf, InstallError> {
    let node_root = prefix.join("node");
    let final_dir = node_root.join(NODE_BOOTSTRAP_VERSION);
    let npm_path = final_dir.join("bin").join("npm");
    if is_executable(&npm_path) {
        return Ok(npm_path);
    }

    let asset =
        node_asset_name(std::env::consts::OS, std::env::consts::ARCH).map_err(InstallError)?;
    create_dir_all(&node_root)?;

    let url = format!("https://nodejs.org/dist/v{NODE_BOOTSTRAP_VERSION}/{asset}");
    on_progress(format!("downloading Node.js v{NODE_BOOTSTRAP_VERSION}"));
    let tarball = http_get(&url)?;

    on_progress("extracting Node.js".to_owned());
    let tmp_dir = node_root.join(format!(".tmp-extract-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    create_dir_all(&tmp_dir)?;
    let decoder = flate2::read::GzDecoder::new(&tarball[..]);
    tar::Archive::new(decoder)
        .unpack(&tmp_dir)
        .map_err(|e| InstallError(format!("extracting node tarball: {e}")))?;

    // The tarball's sole top-level entry is `node-v<ver>-<os>-<arch>/`.
    let extracted = std::fs::read_dir(&tmp_dir)
        .map_err(|e| InstallError(format!("reading {}: {e}", tmp_dir.display())))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|p| p.is_dir())
        .ok_or_else(|| InstallError("node tarball had no top-level directory".to_owned()))?;
    std::fs::rename(&extracted, &final_dir)
        .map_err(|e| InstallError(format!("installing node into {}: {e}", final_dir.display())))?;
    let _ = std::fs::remove_dir_all(&tmp_dir);

    if !is_executable(&npm_path) {
        return Err(InstallError(format!(
            "node bootstrap did not produce {}",
            npm_path.display()
        )));
    }
    Ok(npm_path)
}

// --- Shared plumbing ---------------------------------------------------------

fn create_dir_all(dir: &Path) -> Result<(), InstallError> {
    std::fs::create_dir_all(dir)
        .map_err(|e| InstallError(format!("creating {}: {e}", dir.display())))
}

/// Writes `bytes` to a temp file beside `final_path` and renames it into
/// place — the atomic-install property the module doc comment promises for
/// the single-file strategies (rust-analyzer, and gopls's own binary once
/// `go install` produces it): a reader can never observe a partially
/// written file at `final_path`, because it's never written there directly.
fn atomic_write_executable(final_path: &Path, bytes: &[u8]) -> Result<(), InstallError> {
    let dir = final_path
        .parent()
        .ok_or_else(|| InstallError(format!("{} has no parent directory", final_path.display())))?;
    let file_name = final_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| InstallError(format!("{} has no valid file name", final_path.display())))?;
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", std::process::id()));

    std::fs::write(&tmp_path, bytes)
        .map_err(|e| InstallError(format!("writing {}: {e}", tmp_path.display())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| InstallError(format!("chmod {}: {e}", tmp_path.display())))?;
    }
    std::fs::rename(&tmp_path, final_path)
        .map_err(|e| InstallError(format!("installing {}: {e}", final_path.display())))?;
    Ok(())
}

/// `GET`s `url` and returns its full body — used for the two binary/archive
/// downloads (rust-analyzer, the Node bootstrap tarball), both small enough
/// (tens of megabytes) to buffer entirely in memory rather than stream to
/// disk. Refuses anything not HTTPS to an allowed host before making the
/// request at all — see [`assert_allowed_host`] and the module doc comment.
fn http_get(url: &str) -> Result<Vec<u8>, InstallError> {
    assert_allowed_host(url)?;
    let response = ureq::get(url)
        .call()
        .map_err(|e| InstallError(format!("GET {url}: {e}")))?;
    let mut body = Vec::new();
    response
        .into_reader()
        .read_to_end(&mut body)
        .map_err(|e| InstallError(format!("reading response body from {url}: {e}")))?;
    Ok(body)
}

/// The allowlist backing this module's "HTTPS, official hosts only"
/// guarantee (see the module doc comment): `github.com` for rust-analyzer
/// releases, `nodejs.org` for the Node.js bootstrap,
/// `download-cdn.jetbrains.com` for kotlin-lsp releases, and
/// `download.eclipse.org` for jdtls milestone releases are the only hosts
/// this module's own HTTP client ever talks to directly — npm's registry is
/// reached through the `npm` binary instead, never through here.
fn assert_allowed_host(url: &str) -> Result<(), InstallError> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Err(InstallError(format!("refusing a non-HTTPS URL: {url}")));
    };
    let host = rest.split('/').next().unwrap_or("");
    let allowed = host == "github.com"
        || host == "nodejs.org"
        || host == "download-cdn.jetbrains.com"
        || host == "download.eclipse.org";
    if !allowed {
        return Err(InstallError(format!(
            "refusing a download from an untrusted host: {host}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // --- prefix_dir_from_env -------------------------------------------

    #[test]
    fn prefix_dir_from_env_respects_xdg_data_home() {
        let path = prefix_dir_from_env(
            Some(OsString::from("/custom/data")),
            Some(OsString::from("/home/someone")),
        );
        assert_eq!(path, PathBuf::from("/custom/data/katamari/servers"));
    }

    #[test]
    fn prefix_dir_from_env_falls_back_to_home_local_share_when_xdg_unset() {
        let path = prefix_dir_from_env(None, Some(OsString::from("/home/someone")));
        assert_eq!(
            path,
            PathBuf::from("/home/someone/.local/share/katamari/servers")
        );
    }

    #[test]
    fn prefix_dir_from_env_ignores_an_empty_xdg_data_home() {
        // An exported-but-empty `XDG_DATA_HOME=""` is not meaningfully
        // "set" — falling through to `$HOME` is more useful than joining
        // paths onto an empty string.
        let path = prefix_dir_from_env(Some(OsString::new()), Some(OsString::from("/home/x")));
        assert_eq!(path, PathBuf::from("/home/x/.local/share/katamari/servers"));
    }

    // --- asset-name mapping ---------------------------------------------

    #[test]
    fn rust_analyzer_asset_name_maps_every_supported_target() {
        assert_eq!(
            rust_analyzer_asset_name("macos", "aarch64").unwrap(),
            "rust-analyzer-aarch64-apple-darwin.gz"
        );
        assert_eq!(
            rust_analyzer_asset_name("macos", "x86_64").unwrap(),
            "rust-analyzer-x86_64-apple-darwin.gz"
        );
        assert_eq!(
            rust_analyzer_asset_name("linux", "aarch64").unwrap(),
            "rust-analyzer-aarch64-unknown-linux-gnu.gz"
        );
        assert_eq!(
            rust_analyzer_asset_name("linux", "x86_64").unwrap(),
            "rust-analyzer-x86_64-unknown-linux-gnu.gz"
        );
    }

    #[test]
    fn rust_analyzer_asset_name_reports_unsupported_targets_clearly() {
        let err = rust_analyzer_asset_name("windows", "x86_64").unwrap_err();
        assert!(err.contains("windows/x86_64"));
    }

    #[test]
    fn node_asset_name_maps_every_supported_target() {
        assert_eq!(
            node_asset_name("macos", "aarch64").unwrap(),
            format!("node-v{NODE_BOOTSTRAP_VERSION}-darwin-arm64.tar.gz")
        );
        assert_eq!(
            node_asset_name("linux", "x86_64").unwrap(),
            format!("node-v{NODE_BOOTSTRAP_VERSION}-linux-x64.tar.gz")
        );
    }

    #[test]
    fn node_asset_name_reports_unsupported_targets_clearly() {
        assert!(node_asset_name("windows", "x86_64").is_err());
    }

    #[test]
    fn kotlin_lsp_asset_name_maps_every_supported_target() {
        assert_eq!(
            kotlin_lsp_asset_name("macos", "aarch64").unwrap(),
            format!("kotlin-server-{KOTLIN_LSP_VERSION}-aarch64.sit")
        );
        assert_eq!(
            kotlin_lsp_asset_name("macos", "x86_64").unwrap(),
            format!("kotlin-server-{KOTLIN_LSP_VERSION}.sit")
        );
        assert_eq!(
            kotlin_lsp_asset_name("linux", "aarch64").unwrap(),
            format!("kotlin-server-{KOTLIN_LSP_VERSION}-aarch64.tar.gz")
        );
        assert_eq!(
            kotlin_lsp_asset_name("linux", "x86_64").unwrap(),
            format!("kotlin-server-{KOTLIN_LSP_VERSION}.tar.gz")
        );
    }

    #[test]
    fn kotlin_lsp_asset_name_reports_unsupported_targets_clearly() {
        let err = kotlin_lsp_asset_name("windows", "x86_64").unwrap_err();
        assert!(err.contains("windows/x86_64"));
    }

    // --- host allowlist ---------------------------------------------------

    #[test]
    fn assert_allowed_host_accepts_github_and_nodejs() {
        assert!(assert_allowed_host("https://github.com/foo/bar").is_ok());
        assert!(assert_allowed_host("https://nodejs.org/dist/x").is_ok());
    }

    #[test]
    fn assert_allowed_host_accepts_the_jetbrains_cdn() {
        assert!(
            assert_allowed_host(
                "https://download-cdn.jetbrains.com/language-server/kotlin-server/x"
            )
            .is_ok()
        );
    }

    #[test]
    fn assert_allowed_host_accepts_the_eclipse_download_host() {
        assert!(
            assert_allowed_host("https://download.eclipse.org/jdtls/milestones/1.60.0/x").is_ok()
        );
    }

    #[test]
    fn assert_allowed_host_rejects_plain_http() {
        let err = assert_allowed_host("http://github.com/foo").unwrap_err();
        assert!(err.to_string().contains("non-HTTPS"));
    }

    #[test]
    fn assert_allowed_host_rejects_an_untrusted_host() {
        let err = assert_allowed_host("https://evil.example.com/rust-analyzer").unwrap_err();
        assert!(err.to_string().contains("untrusted"));
    }

    // --- installed_binary_path / idempotency -----------------------------

    #[test]
    fn installed_binary_path_is_none_when_nothing_is_there() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(installed_binary_path(dir.path(), Language::Rust), None);
    }

    #[cfg(unix)]
    #[test]
    fn installed_binary_path_finds_an_executable_at_the_expected_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Rust);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        make_executable(&path);

        assert_eq!(
            installed_binary_path(dir.path(), Language::Rust),
            Some(path)
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_binary_path_finds_kotlin_lsp_at_its_renamed_layout() {
        // Kotlin's `binary_path` doesn't embed the per-arch asset directory
        // name (`kotlin-server-<version>-aarch64/`) — `install_kotlin_lsp`
        // renames that into `<prefix>/kotlin-lsp/<version>/` on install, so
        // this must resolve regardless of which platform produced it.
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Kotlin);
        assert!(path.ends_with("kotlin-lsp/262.9593.0/bin/intellij-server"));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        make_executable(&path);

        assert_eq!(
            installed_binary_path(dir.path(), Language::Kotlin),
            Some(path)
        );
    }

    #[test]
    fn binary_path_joins_the_pinned_launcher_jar_under_plugins() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Java);
        assert!(path.ends_with(format!(
            "jdt-language-server/{JDTLS_VERSION}/plugins/{JDTLS_LAUNCHER_JAR}"
        )));
    }

    #[cfg(unix)]
    #[test]
    fn installed_binary_path_finds_jdtls_at_a_fake_managed_layout() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Java);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"PK\x03\x04fake jar").unwrap();
        make_executable(&path);

        assert_eq!(
            installed_binary_path(dir.path(), Language::Java),
            Some(path)
        );
    }

    #[cfg(unix)]
    #[test]
    fn installed_binary_path_ignores_a_non_executable_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Go);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not executable").unwrap(); // default mode: no +x

        assert_eq!(installed_binary_path(dir.path(), Language::Go), None);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_in_returns_an_already_installed_binary_without_any_install_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Rust);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"#!/bin/sh\necho fake\n").unwrap();
        make_executable(&path);

        let mut progress_calls = 0;
        let result = ensure_in(dir.path(), Language::Rust, |_| progress_calls += 1);

        assert_eq!(result.unwrap(), path);
        assert_eq!(
            progress_calls, 0,
            "no progress callback fired means install work (and any network call) was never attempted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_in_returns_an_already_installed_kotlin_lsp_without_any_install_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Kotlin);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"#!/bin/sh\necho fake\n").unwrap();
        make_executable(&path);

        let mut progress_calls = 0;
        let result = ensure_in(dir.path(), Language::Kotlin, |_| progress_calls += 1);

        assert_eq!(result.unwrap(), path);
        assert_eq!(
            progress_calls, 0,
            "no progress callback fired means install work (and any network call) was never attempted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ensure_in_returns_an_already_installed_jdtls_without_any_install_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = binary_path(dir.path(), Language::Java);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"PK\x03\x04fake jar").unwrap();
        make_executable(&path);

        let mut progress_calls = 0;
        let result = ensure_in(dir.path(), Language::Java, |_| progress_calls += 1);

        assert_eq!(result.unwrap(), path);
        assert_eq!(
            progress_calls, 0,
            "no progress callback fired means install work (and any network call) was never attempted"
        );
    }

    // --- extract_archive ---------------------------------------------------

    /// Builds a minimal in-memory zip containing one file at
    /// `<top>/bin/intellij-server`, with its unix mode bits set — the same
    /// shape kotlin-lsp's real macOS `.sit` release has (verified by hand
    /// against the actual asset; see `install_kotlin_lsp`'s docs).
    fn build_test_zip(top: &str) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let options = zip::write::SimpleFileOptions::default().unix_permissions(0o755);
        writer
            .start_file(format!("{top}/bin/intellij-server"), options)
            .unwrap();
        std::io::Write::write_all(&mut writer, b"#!/bin/sh\necho fake\n").unwrap();
        writer.finish().unwrap();
        buf
    }

    #[test]
    fn extract_archive_dispatches_zip_by_suffix_and_preserves_unix_mode() {
        let bytes = build_test_zip("kotlin-server-262.9593.0-aarch64");
        let dir = tempfile::tempdir().unwrap();
        extract_archive("kotlin-server-262.9593.0-aarch64.sit", &bytes, dir.path()).unwrap();

        let extracted = dir
            .path()
            .join("kotlin-server-262.9593.0-aarch64")
            .join("bin")
            .join("intellij-server");
        assert!(extracted.is_file());
        #[cfg(unix)]
        assert!(
            is_executable(&extracted),
            "unix mode from the zip entry should have been preserved"
        );
    }

    #[test]
    fn extract_archive_dispatches_tar_gz_by_suffix() {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            let data = b"#!/bin/sh\necho fake\n";
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o755);
            header.set_cksum();
            builder
                .append_data(
                    &mut header,
                    "kotlin-server-262.9593.0/bin/intellij-server",
                    &data[..],
                )
                .unwrap();
            builder.finish().unwrap();
        }
        let mut gz_bytes = Vec::new();
        {
            let mut encoder =
                flate2::write::GzEncoder::new(&mut gz_bytes, flate2::Compression::fast());
            std::io::Write::write_all(&mut encoder, &tar_bytes).unwrap();
            encoder.finish().unwrap();
        }

        let dir = tempfile::tempdir().unwrap();
        extract_archive("kotlin-server-262.9593.0.tar.gz", &gz_bytes, dir.path()).unwrap();

        let extracted = dir
            .path()
            .join("kotlin-server-262.9593.0")
            .join("bin")
            .join("intellij-server");
        assert!(extracted.is_file());
    }

    // --- real-network smoke test, run manually only -----------------------

    /// Exercises the actual rust-analyzer download end to end. Deliberately
    /// `#[ignore]`d: `cargo test` must stay hermetic and network-free. Run
    /// it explicitly with `cargo test -- --ignored install_rust_analyzer`.
    #[test]
    #[ignore = "hits the real network (github.com); run manually"]
    fn install_rust_analyzer_downloads_and_extracts_a_runnable_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        let path = ensure_in(dir.path(), Language::Rust, |m| messages.push(m)).unwrap();
        assert!(
            !messages.is_empty(),
            "expected progress messages along the way"
        );
        let output = Command::new(&path).arg("--version").output().unwrap();
        assert!(output.status.success());
    }

    /// Exercises the actual kotlin-lsp download end to end — verified by
    /// hand during this language's implementation (downloaded the real
    /// asset, confirmed it's a zip despite the `.sit` extension, confirmed
    /// unix permissions survive extraction, ran the extracted launcher's
    /// `--help`). Deliberately `#[ignore]`d for the same reason as
    /// `install_rust_analyzer_downloads_and_extracts_a_runnable_binary`:
    /// `cargo test` must stay hermetic and network-free, and this downloads
    /// several hundred megabytes. Run it explicitly with `cargo test --
    /// --ignored install_kotlin_lsp`.
    #[test]
    #[ignore = "hits the real network (download-cdn.jetbrains.com); run manually"]
    fn install_kotlin_lsp_downloads_and_extracts_a_runnable_binary() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        let path = ensure_in(dir.path(), Language::Kotlin, |m| messages.push(m)).unwrap();
        assert!(
            !messages.is_empty(),
            "expected progress messages along the way"
        );
        let output = Command::new(&path).arg("--help").output().unwrap();
        assert!(output.status.success());
    }

    /// Exercises the actual jdtls download end to end — downloads the real
    /// 50MB+ milestone tarball and asserts the pinned launcher jar lands
    /// exactly where `JDTLS_LAUNCHER_JAR`/`binary_path` say it should.
    /// Deliberately `#[ignore]`d for the same reason as the other two
    /// real-network tests in this module: `cargo test` must stay hermetic
    /// and network-free. Run it explicitly with `cargo test -- --ignored
    /// install_jdtls`.
    #[test]
    #[ignore = "hits the real network (download.eclipse.org); run manually"]
    fn install_jdtls_downloads_and_extracts_the_pinned_launcher_jar() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        let path = ensure_in(dir.path(), Language::Java, |m| messages.push(m)).unwrap();
        assert!(
            !messages.is_empty(),
            "expected progress messages along the way"
        );
        assert!(path.ends_with(JDTLS_LAUNCHER_JAR));
        assert!(path.is_file());
    }
}
