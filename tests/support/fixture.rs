//! Throwaway git repositories for the E2E suite to point `ktmr diff` at.
//! Every fixture uses plain `.txt`/`.md` content deliberately — never
//! `.rs`/`.ts`/`.py`/`.go` — so [`katamari`'s `Language::detect`] never
//! matches, which means [`LspManager::warm_up`] never has anything eligible
//! to open and no language server is ever resolved, installed, or spawned.
//! An E2E suite that accidentally depended on network access (an
//! auto-install) or a locally installed toolchain would be flaky in exactly
//! the ways this milestone is trying to eliminate.
//!
//! [`lsp_readiness_repo`]/[`lsp_readiness_repo_with_definition_target`] are
//! the one deliberate exception: issue #11's readiness coverage needs a
//! real, controllable server lifecycle to press hover/definition/references
//! against while it's still starting, which no built-in language could give
//! this suite without an auto-install, network access, or a locally
//! installed toolchain — exactly what every other fixture here exists to
//! avoid. They stay safe the same way a custom `[lsp.servers.<id>]` entry
//! always is: a fabricated file extension no built-in
//! [`katamari`'s `Language::detect`] has ever heard of, routed to a small
//! script this repository controls end to end (see
//! `support::fake_lsp_server`'s docs) rather than a real language server.

use std::path::Path;
use std::process::Command;

/// A repository directory that outlives the [`tempfile::TempDir`] guard
/// dropped alongside it — kept together so a fixture's lifetime is tied to
/// the value a test holds, the same RAII pattern [`tempfile::TempDir`]
/// itself uses.
pub struct FixtureRepo {
    dir: tempfile::TempDir,
}

impl FixtureRepo {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `git rev-parse --short <rev>`'s output — for a test that needs a
    /// deterministic revision to type into the scope-menu popup's
    /// "Revision…" input. Commit hashes aren't deterministic across runs
    /// (they hash the commit's author/committer timestamps among other
    /// things), so a test can never hardcode one; it has to ask the fixture
    /// for whatever hash it actually produced.
    pub fn commit_hash(&self, rev: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", "--short", rev])
            .current_dir(self.path())
            .output()
            .expect("failed to spawn git — is it on PATH?");
        assert!(
            output.status.success(),
            "git rev-parse --short {rev} failed in {}:\nstderr: {}",
            self.path().display(),
            String::from_utf8_lossy(&output.stderr),
        );
        String::from_utf8(output.stdout)
            .expect("git rev-parse produced non-UTF-8 output")
            .trim()
            .to_owned()
    }
}

/// Runs `git <args>` in `dir` with a fixed, throwaway identity (`-c
/// user.email`/`-c user.name`, not a real `~/.gitconfig`, which the test
/// process's real environment may or may not have configured) and panics
/// with the captured output on failure — a fixture that failed to build is a
/// test-infrastructure bug, not something a test body should have to check
/// for.
fn git(dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args([
            "-c",
            "user.email=e2e@katamari.test",
            "-c",
            "user.name=katamari e2e",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("failed to spawn git — is it on PATH?");
    assert!(
        output.status.success(),
        "git {args:?} failed in {}:\nstdout: {}\nstderr: {}",
        dir.display(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("katamari-e2e-repo-")
        .tempdir()
        .expect("failed to create fixture tempdir");
    git(dir.path(), &["init", "-q"]);
    dir
}

/// A small repo with an initial commit plus uncommitted working-tree edits
/// to plain-text files — enough content for `ktmr diff` to show a real diff
/// (added/changed/context lines, more than one file) without pulling in any
/// language-specific machinery.
pub fn basic_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(
        root.join("README.md"),
        "# Sample project\n\nThis is line two.\nThis is line three.\nThis is line four.\n",
    )
    .unwrap();
    std::fs::write(root.join("notes.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    // Working-tree edits: one changed file, one new (untracked) file — the
    // shape a reviewer's session normally opens onto.
    std::fs::write(
        root.join("README.md"),
        "# Sample project\n\nThis is line two, updated.\nThis is line three.\nThis is line four.\nA brand new line five.\n",
    )
    .unwrap();
    std::fs::write(root.join("todo.txt"), "- write more fixtures\n").unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit touches Japanese (CJK, double-width)
/// content — the first end-to-end guard for the whole rendering pipeline
/// (parse -> highlight -> terminal cells) on wide characters, not just the
/// unit-level `is_wide`/`display_width` tests that already exist.
pub fn japanese_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(
        root.join("greeting.txt"),
        "hello\nこんにちは世界\ngoodbye\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    std::fs::write(
        root.join("greeting.txt"),
        "hello\nこんにちは世界、これは日本語のテストです\ngoodbye\n",
    )
    .unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit adds a single, very long content line —
/// the M13 wrap E2E fixture. The line is built to an exact, known shape:
/// 100 display columns of prefix (70 ASCII dashes then 15 double-width
/// Japanese characters, mixing both the way a real diff line would), then a
/// `TAILMARKER` word found nowhere else in the fixture. At the default
/// 100-column terminal `tests/e2e/wrap.rs`'s two tests spawn, with the
/// sidebar showing, `[ui] wrap`'s rendered content width works out to 49
/// columns (`100 - 30 sidebar - 2 border - 19 gutter` — see
/// `diff_view::unified_content_width`/`gutter_width`; issue #14 grew the
/// diff pane's border from 1 column, a bare left rule, to 2, a full
/// `PaneChrome` box — `unified_content_width` derives this from
/// `pane::inner_rect` rather than hand-counting it, so this comment is the
/// only place that number is written down at all). `TAILMARKER` (columns
/// 101-110) still lands fully intact on the third wrapped row (columns
/// 99-147) at that width — 49 no longer divides 100 evenly the way the
/// pre-#14 width of 50 did, so it's no longer aligned to that row's very
/// first column, but it still fits inside it — or never renders at all
/// when truncated: still an unambiguous, deterministic witness either way.
/// At *other* terminal widths this alignment isn't guaranteed — narrow
/// enough and `TAILMARKER` itself can land split across two wrapped rows
/// (`tests/e2e/focus.rs`'s narrow-terminal case hits exactly this and
/// checks for the wrap marker glyph instead of the word itself). `wrap` is
/// always written into `.katamari/config.toml` explicitly (never left to
/// the built-in default), so the fixture's behavior doesn't silently
/// depend on what that default happens to be.
pub fn long_line_repo(wrap: bool) -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("long.txt"), "one\ntwo\nthree\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    let prefix = format!("{}{}", "-".repeat(70), "日本語のテキストで幅を確認する");
    let long_line = format!("{prefix}TAILMARKER");
    std::fs::write(root.join("long.txt"), format!("one\n{long_line}\nthree\n")).unwrap();

    std::fs::create_dir_all(root.join(".katamari")).unwrap();
    std::fs::write(
        root.join(".katamari").join("config.toml"),
        format!("[ui]\nwrap = {wrap}\n"),
    )
    .unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit touches two lines far apart in one
/// 40-line file (line 3 and line 35) — far enough apart that git's default
/// 3-line unified context leaves a real, non-adjacent gap between the two
/// resulting hunks. `GAPMARKER_UNIQUE_TEXT` sits at line 20, squarely
/// inside that gap and nowhere else in the fixture, so its (dis)appearance
/// on screen is an unambiguous witness for whether the fold row is
/// expanded or collapsed. The Issue #2 fold E2E test (`tests/e2e/fold.rs`)
/// is the one thing that cares about this exact shape — see that file for
/// the row-layout arithmetic it depends on.
pub fn fold_gap_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    let mut lines: Vec<String> = (1..=40).map(|n| format!("line {n}")).collect();
    lines[19] = "GAPMARKER_UNIQUE_TEXT".to_owned();
    std::fs::write(root.join("big.txt"), lines.join("\n") + "\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    lines[2] = "line 3 CHANGED".to_owned();
    lines[34] = "line 35 CHANGED".to_owned();
    std::fs::write(root.join("big.txt"), lines.join("\n") + "\n").unwrap();

    FixtureRepo { dir }
}

/// A repo whose working-tree edit appends 60 consecutive new lines to one
/// file — all part of a single hunk (unlike [`fold_gap_repo`], nothing here
/// is far enough from a change to leave a git-omitted gap, so every line is
/// real, searchable `RenderRow::Line` content, never a fold row). One of
/// them, near the bottom of the appended block, is
/// `SEARCH_TARGET_UNIQUE` — the Issue #5 search E2E suite's witness that
/// `/` actually moved the viewport to reveal it, rather than something
/// already on screen: a 30-row terminal's diff pane shows nowhere near 63
/// rows (`FileHeader` + `HunkHeader` + 3 unchanged intro lines + 60 added
/// lines) without scrolling.
pub fn search_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(
        root.join("search.txt"),
        "intro line one\nintro line two\nintro line three\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    let mut appended: Vec<String> = (1..=60).map(|n| format!("filler line {n}")).collect();
    appended[54] = "SEARCH_TARGET_UNIQUE appears here".to_owned();
    let content = format!(
        "intro line one\nintro line two\nintro line three\n{}\n",
        appended.join("\n")
    );
    std::fs::write(root.join("search.txt"), content).unwrap();

    FixtureRepo { dir }
}

/// A repo whose sole working-tree edit adds `main.stub` — an extension
/// nothing built into katamari has ever heard of — paired with a
/// `.katamari/config.toml` `[lsp.servers.stubls]` entry that claims `.stub`
/// and points at `support::fake_lsp_server`'s script (see the module docs
/// above for why this is the one fixture allowed to make katamari actually
/// spawn something). The committed and working-tree content share the
/// first two lines (`alpha`/`beta`, both `Context` rows once diffed) with
/// `GOTO_TARGET_TOKEN` added as a third — any of the three is a valid
/// hover/goto target (katamari's `App::hover_query` accepts `Context` and
/// `Add` rows alike), so a test can press an action from wherever `j`/`k`
/// lands the cursor without needing to know the diff's exact row layout.
///
/// `init_delay_secs` is how long the fake server waits before answering
/// `initialize` — the window a test presses actions during to prove they
/// report "not ready" instead of queuing. `definition_delay_secs` is how
/// long it waits before answering `textDocument/definition` once `Ready` —
/// for a test that wants to prove movement stays responsive while a
/// *ready* server is still slow to answer the request itself.
/// Whether the fake-LSP fixture can run at all: `fake_lsp_server.py` needs
/// a `python3` on `$PATH`, the one external dependency this otherwise
/// hermetic support module has (see the module doc). Tests that spawn
/// [`lsp_readiness_repo`] should skip (with an eprintln, so the skip is
/// visible in `--nocapture` runs) rather than fail when it's missing —
/// a contributor without python3 would otherwise see the spawn collapse
/// into `Unavailable: command not found` and the test time out waiting
/// for "is starting" text that can never appear, which reads like a
/// katamari regression instead of a missing tool.
pub fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_ok()
}

/// A repo shaped for issue #15's file-tree PTY coverage: a changed file
/// nested at least two directories deep, a deleted file, a renamed file,
/// and a large padding file placed alphabetically (and therefore
/// diff-order) first. `tests/e2e/file_tree.rs` spawns `ktmr diff --staged`
/// against this fixture, not the plain working-tree default: katamari's own
/// `GitSource::working_tree_diff` combines plain `git diff` (tracked,
/// unstaged changes) with a *separate* `--no-index` diff per untracked
/// file, so an unstaged rename (an untracked new path plus a missing
/// tracked one) can never pair up into one `rename from`/`rename to` entry
/// the way a *staged* rename does — only `git diff --cached` (what
/// `--staged` shows) ever produces one.
///
/// `src/aaa_padding.txt`'s only job is bulk: 60 added lines, sorted (and
/// therefore diffed) ahead of everything else, so the nested marker file's
/// own `FileHeader` row lands well below a real terminal's initial diff-pane
/// viewport at the default `SpawnOptions` size. That's what makes
/// "`NESTED_MARKER_UNIQUE` leaves the screen when its directory collapses"
/// an unambiguous witness for the *sidebar's* own collapse state rather
/// than a coincidence of whatever the diff pane happens to have scrolled to
/// — collapsing a tree row never changes the diff pane's own content, only
/// the sidebar's.
pub fn tree_repo() -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();
    let src = root.join("src");
    let nested = src.join("nested").join("deep");
    std::fs::create_dir_all(&nested).unwrap();

    std::fs::write(src.join("doomed.txt"), "going away soon\n").unwrap();
    std::fs::write(
        src.join("old_name.txt"),
        "renamed content\nline two\nline three\n",
    )
    .unwrap();
    std::fs::write(nested.join("NESTED_MARKER_UNIQUE.txt"), "before\nafter\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    let padding: String = (1..=60).map(|n| format!("padding line {n}\n")).collect();
    std::fs::write(src.join("aaa_padding.txt"), padding).unwrap();

    std::fs::remove_file(src.join("doomed.txt")).unwrap();

    std::fs::write(
        nested.join("NESTED_MARKER_UNIQUE.txt"),
        "before\nMARKER_CONTENT_LINE_UNIQUE\nafter\n",
    )
    .unwrap();

    std::fs::rename(src.join("old_name.txt"), src.join("new_name.txt")).unwrap();

    // Everything above must be staged — see this function's own docs on
    // why a rename only survives as one diff entry once it's in the index.
    git(root, &["add", "-A"]);

    FixtureRepo { dir }
}

/// `count` small top-level files, each with one changed line — issue #20's
/// mouse E2E fixture. Two independent things need to overflow a default
/// (100x30) terminal at once here: the *sidebar*, so wheel-scrolling it is
/// observable (`count` flat top-level files means `count` sidebar rows, no
/// synthetic directory row to pad past the terminal's usable ~26 with), and
/// the *diff pane*, so wheel-scrolling it moves real content into view
/// (`count` files at ~5 rendered rows each — file header, hunk header,
/// context/changed/context — comfortably clears a 30-row screen well
/// before `count` on its own would). `FIRST_MARKER`/`LAST_MARKER` sit in
/// the first and last files respectively (alphabetical == diff order here,
/// since every name is zero-padded to sort correctly) — unambiguous
/// witnesses for "did the diff pane's visible window actually move," the
/// same role `SEARCH_TARGET_UNIQUE` plays in [`search_repo`].
pub fn many_files_repo(count: usize) -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    for i in 0..count {
        std::fs::write(root.join(format!("file{i:03}.txt")), "alpha\nbeta\ngamma\n").unwrap();
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    for i in 0..count {
        let marker = match i {
            0 => "FIRST_MARKER",
            n if n == count - 1 => "LAST_MARKER",
            _ => "CHANGED",
        };
        std::fs::write(
            root.join(format!("file{i:03}.txt")),
            format!("alpha {marker}\nbeta\ngamma\n"),
        )
        .unwrap();
    }

    FixtureRepo { dir }
}

pub fn lsp_readiness_repo(init_delay_secs: f64, definition_delay_secs: f64) -> FixtureRepo {
    lsp_readiness_repo_inner(init_delay_secs, definition_delay_secs, false)
}

/// As [`lsp_readiness_repo`], but `textDocument/definition` answers with a
/// real [`lsp_types::Location`] in a second, unmodified file (`other.stub`)
/// instead of `None` — issue #12's Esc-from-a-definition-opened-`FileView`
/// PTY coverage needs a genuine `FileView` push, which a `null` result (or
/// a location inside `main.stub` itself, already part of this diff — see
/// `App::row_for_target`) can't produce. Kept as a separate function rather
/// than a third parameter every existing caller would have to thread
/// through: issue #11's not-ready/no-result assertions never have to think
/// about this at all.
///
/// [`lsp_types::Location`]: https://docs.rs/lsp-types/latest/lsp_types/struct.Location.html
pub fn lsp_readiness_repo_with_definition_target(
    init_delay_secs: f64,
    definition_delay_secs: f64,
) -> FixtureRepo {
    lsp_readiness_repo_inner(init_delay_secs, definition_delay_secs, true)
}

fn lsp_readiness_repo_inner(
    init_delay_secs: f64,
    definition_delay_secs: f64,
    definition_target: bool,
) -> FixtureRepo {
    let dir = init_repo();
    let root = dir.path();

    std::fs::write(root.join("main.stub"), "alpha\nbeta\n").unwrap();
    // Committed once, never touched again — absent from the working-tree
    // diff `main.stub`'s own edit below produces, so a jump into it can
    // only ever be a real navigation target, never "coincidentally already
    // part of this diff" (see `lsp_readiness_repo_with_definition_target`'s
    // docs).
    std::fs::write(
        root.join("other.stub"),
        "target line one\ntarget line two\n",
    )
    .unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-q", "-m", "initial commit"]);

    std::fs::write(root.join("main.stub"), "alpha\nbeta\nGOTO_TARGET_TOKEN\n").unwrap();

    let server_script = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/support/fake_lsp_server.py"
    );
    std::fs::create_dir_all(root.join(".katamari")).unwrap();
    std::fs::write(
        root.join(".katamari").join("config.toml"),
        format!(
            "[lsp.servers.stubls]\n\
             command = \"python3\"\n\
             args = [\"{server_script}\", \"{init_delay_secs}\", \"{definition_delay_secs}\", \"{}\"]\n\
             extensions = [\"stub\"]\n",
            if definition_target { "1" } else { "0" },
        ),
    )
    .unwrap();

    FixtureRepo { dir }
}
