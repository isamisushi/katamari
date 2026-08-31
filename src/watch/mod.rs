//! Watches a repository's working tree for changes and turns them into
//! debounced batches [`crate::ui`]'s event loop can refresh a diff review
//! from, without the reviewer ever pressing a key. Three concerns, each
//! kept separate: [`debounce`] owns purely the "when has this burst of
//! events settled" arithmetic (independently unit-testable, no clock or
//! thread of its own); this module owns turning raw `notify` events into
//! [`ChangedPath`]s (filtering out `.git/`, build output, and gitignored
//! paths along the way) and running that pipeline on its own thread; the
//! caller (`ui::mod`) owns what a flushed batch actually triggers.

pub mod debounce;

pub use debounce::{ChangeKind, ChangedPath};

use crate::vcs::git::GitSource;
use debounce::Debounce;
use ignore::Match;
use ignore::gitignore::Gitignore;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

/// Trailing-edge quiet period: a batch flushes this long after the last
/// change in it, so a save that touches several files in quick succession
/// (a formatter rewriting an entire crate, an editor's atomic-rename save)
/// becomes one refresh, not one per file.
pub const DEBOUNCE_QUIET: Duration = Duration::from_millis(200);

/// Backstop so a continuous stream of writes (a build loop, a very chatty
/// file watcher upstream of this one) still refreshes periodically instead
/// of never going quiet — see [`Debounce`]'s docs.
pub const DEBOUNCE_MAX_LATENCY: Duration = Duration::from_secs(1);

/// How often the watcher thread wakes up to check whether the current
/// debounce window should flush, when no new filesystem event is what wakes
/// it instead. Small relative to [`DEBOUNCE_QUIET`] so the actual flush
/// happens close to the nominal 200ms/1s deadlines rather than lagging by
/// most of a tick.
const POLL_TICK: Duration = Duration::from_millis(50);

/// Directory names excluded regardless of `.gitignore` — version control
/// metadata, build output, and dependency caches that are never useful to
/// re-diff or resync a language server over, and for `.git`/`.jj` in
/// particular, directories a plain gitignore lookup wouldn't even cover
/// (git doesn't gitignore its own `.git/`).
const HARDCODED_EXCLUDES: &[&str] = &[".git", ".jj", ".katamari", "target", "node_modules"];

/// One flushed, debounced set of filesystem changes.
pub struct WatchBatch {
    pub changes: Vec<ChangedPath>,
}

/// How long [`spawn_comments_watcher`] waits after one signal before it will
/// send another — small, since a comment-log append is a tiny, infrequent
/// write (nothing like the "formatter rewrites a whole crate" burst
/// [`DEBOUNCE_QUIET`] exists for), but still enough to collapse the couple
/// of raw filesystem events a single `write()` can generate on some
/// platforms into one reload. A write landing *inside* the window defers
/// the next signal to the window's expiry rather than being dropped — see
/// [`CommentsWatchSession::run`].
const COMMENTS_DEBOUNCE: Duration = Duration::from_millis(100);

/// What the watcher thread sends: either a flushed batch, or a lightweight
/// heads-up that a debounce window just opened (the first change since the
/// last flush) — purely a UI hint so a status bar can show "something's
/// changing" before the batch it belongs to is ready; nothing downstream
/// treats it as anything more than that.
pub enum WatchSignal {
    Pending,
    Flushed(WatchBatch),
}

/// Starts watching `repo_root` and sends [`WatchSignal`]s to `tx` until the
/// receiving end goes away. The initial `notify` setup (spawning the
/// platform watcher, registering the recursive watch) happens synchronously
/// so a caller finds out immediately if it failed — e.g. `repo_root`
/// doesn't exist, or the platform's file-watching backend can't be
/// initialized — rather than only via a batch that silently never arrives;
/// the debounce loop itself then runs on its own thread for the rest of the
/// session.
/// `quiet` is the trailing-edge debounce window (config's `[watch]
/// debounce_ms`, default [`DEBOUNCE_QUIET`]) — how long a burst of changes
/// must go silent before it flushes. [`DEBOUNCE_MAX_LATENCY`]'s backstop is
/// not configurable: it exists to bound worst-case staleness under a
/// continuous stream of writes, a concern orthogonal to how snappy an
/// ordinary quiet-period flush feels, which is the only thing `quiet`
/// tunes.
pub fn spawn(repo_root: PathBuf, tx: Sender<WatchSignal>, quiet: Duration) -> notify::Result<()> {
    let session = WatchSession::start(repo_root)?;
    std::thread::spawn(move || session.run(tx, quiet));
    Ok(())
}

/// One watcher connection: the underlying OS watcher (kept alive only for
/// its `Drop`, never read from directly — see `notify_rx`), the gitignore
/// filter built once at start, and the debounce state that accumulates
/// across the session.
struct WatchSession {
    repo_root: PathBuf,
    ignore: Gitignore,
    notify_rx: mpsc::Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher,
}

impl WatchSession {
    fn start(repo_root: PathBuf) -> notify::Result<Self> {
        let ignore = build_ignore(&repo_root);
        let (notify_tx, notify_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        })?;
        watcher.watch(&repo_root, RecursiveMode::Recursive)?;
        Ok(Self {
            repo_root,
            ignore,
            notify_rx,
            _watcher: watcher,
        })
    }

    /// Runs the classify → filter → debounce pipeline until `tx`'s receiver
    /// is dropped (the normal way this ends: the TUI session exited) or the
    /// underlying `notify` channel disconnects (the platform watcher itself
    /// died, which nothing currently recovers from — a watch session that
    /// silently stopped noticing changes would be worse than one that's
    /// visibly gone).
    fn run(self, tx: Sender<WatchSignal>, quiet: Duration) {
        let start = Instant::now();
        let mut debounce = Debounce::new(quiet, DEBOUNCE_MAX_LATENCY);

        loop {
            match self.notify_rx.recv_timeout(POLL_TICK) {
                Ok(Ok(event)) => {
                    for (path, kind) in classify(&event) {
                        if is_excluded(&self.repo_root, &path, &self.ignore) {
                            continue;
                        }
                        let opening_window = debounce.is_empty();
                        debounce.record(start.elapsed(), path, kind);
                        if opening_window && tx.send(WatchSignal::Pending).is_err() {
                            return;
                        }
                    }
                }
                Ok(Err(_)) => {
                    // A single event failed to decode/deliver on the
                    // platform backend's side; nothing about the session
                    // as a whole is invalid, so keep going rather than
                    // tearing down the watch over one bad event.
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }

            if debounce.should_flush(start.elapsed()) {
                let changes = debounce.flush();
                if !changes.is_empty()
                    && tx
                        .send(WatchSignal::Flushed(WatchBatch { changes }))
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// Classifies one `notify` event into the [`ChangeKind`]s this module cares
/// about, paired with every path the event names. Access/`Any`/`Other`
/// events (file opens, and backend-specific catch-alls) carry nothing a
/// diff refresh or an LSP resync would act on, so they classify to nothing.
fn classify(event: &Event) -> Vec<(PathBuf, ChangeKind)> {
    let kind = match event.kind {
        EventKind::Create(_) => ChangeKind::Created,
        EventKind::Modify(_) => ChangeKind::Changed,
        EventKind::Remove(_) => ChangeKind::Deleted,
        EventKind::Access(_) | EventKind::Any | EventKind::Other => return Vec::new(),
    };
    event.paths.iter().cloned().map(|p| (p, kind)).collect()
}

/// Whether `path` (absolute, somewhere under `repo_root`) should never
/// reach the debounce window at all: inside a [`HARDCODED_EXCLUDES`]
/// directory, or matched by the repository's root `.gitignore`. Paths
/// outside `repo_root` entirely (shouldn't happen — the watch is rooted
/// there — but `notify`'s platform backends have occasionally been known to
/// report a symlink-resolved path that doesn't share the watched root's
/// prefix) fail open rather than being silently dropped, since a missed
/// refresh is a worse failure mode than an extra one.
fn is_excluded(repo_root: &Path, path: &Path, ignore: &Gitignore) -> bool {
    let Ok(relative) = path.strip_prefix(repo_root) else {
        return false;
    };
    let in_hardcoded_exclude = relative.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| HARDCODED_EXCLUDES.contains(&name))
    });
    if in_hardcoded_exclude {
        return true;
    }
    let is_dir = path.is_dir();
    matches!(
        ignore.matched_path_or_any_parents(relative, is_dir),
        Match::Ignore(_)
    )
}

/// Watches `<repo_root>/.katamari/` for changes to `comments.jsonl` and
/// sends a signal on `tx` each time it does — the M6 secondary watch the
/// milestone spec calls for: independent of [`spawn`] (which excludes all
/// of `.katamari/` from the *diff*-refresh watch — see
/// [`HARDCODED_EXCLUDES`] — since a comment being added or resolved is
/// never a reason to re-run `git diff`) and independent of root live-refresh
/// mode entirely. `ui::run` starts this unconditionally for any session with
/// a root diff, so live comment reload works in a plain `ktmr diff` just as
/// much as it does in any other diff session.
///
/// Creates `.katamari/` (and its `.gitignore`) if it doesn't exist yet
/// (nobody has written a comment in this repo before) so there's always a
/// directory to watch — `notify` can't watch a path that isn't there. Goes
/// through [`crate::comments::CommentStore::ensure_dir`] rather than a raw
/// `create_dir_all` so this — the watcher, which starts unconditionally on
/// every `ktmr diff` and so is usually what actually creates `.katamari/`
/// first — seeds the `.gitignore` too, instead of leaving that to whichever
/// comment gets appended first (which may never happen in a session where
/// nobody comments).
pub fn spawn_comments_watcher(repo_root: PathBuf, tx: Sender<()>) -> notify::Result<()> {
    let session = CommentsWatchSession::start(repo_root)?;
    std::thread::spawn(move || session.run(tx));
    Ok(())
}

struct CommentsWatchSession {
    comments_path: PathBuf,
    notify_rx: mpsc::Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher,
}

impl CommentsWatchSession {
    fn start(repo_root: PathBuf) -> notify::Result<Self> {
        // Deriving the watched directory and target filename from
        // `CommentStore::path` (rather than hardcoding `.katamari`/
        // `comments.jsonl` here too) keeps the on-disk layout decision
        // owned by exactly one place — see that method's docs.
        let store = crate::comments::CommentStore::new(&repo_root);
        let comments_path = store.path().to_path_buf();
        let dir = comments_path
            .parent()
            .expect("comments.jsonl always has a parent")
            .to_path_buf();
        store
            .ensure_dir()
            .map_err(|e| notify::Error::generic(&e.to_string()))?;

        let (notify_tx, notify_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        })?;
        watcher.watch(&dir, RecursiveMode::NonRecursive)?;
        Ok(Self {
            comments_path,
            notify_rx,
            _watcher: watcher,
        })
    }

    fn run(self, tx: Sender<()>) {
        let mut last_sent: Option<Instant> = None;
        // A relevant event that lands inside the debounce window is
        // *deferred*, never dropped: comment-log writes are routinely
        // machine-paced (a scripted reviewer running `ktmr comments add`
        // then `resolve` back to back — the repo's own agent workflow),
        // so "two distinct writes inside 100ms" is an ordinary sequence
        // whose second half must still reach the session. The flush check
        // below runs on every loop iteration — the `recv_timeout` tick
        // already wakes this loop every [`POLL_TICK`] — so a deferred
        // signal goes out as soon as the window expires, no extra timer.
        let mut deferred = false;
        let target_name = self.comments_path.file_name();
        loop {
            match self.notify_rx.recv_timeout(POLL_TICK) {
                Ok(Ok(event)) => {
                    let touches_comments =
                        matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                            && event.paths.iter().any(|p| p.file_name() == target_name);
                    if touches_comments {
                        deferred = true;
                    }
                }
                Ok(Err(_)) => {
                    // As in `WatchSession::run`: one bad event from the
                    // platform backend doesn't invalidate the session.
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Leading edge preserved: the first event after a quiet spell
            // reaches here in its own iteration with the window long
            // expired, so it still forwards immediately.
            if deferred && last_sent.is_none_or(|t| t.elapsed() >= COMMENTS_DEBOUNCE) {
                last_sent = Some(Instant::now());
                deferred = false;
                if tx.send(()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Issue #8: a git-dir path this module watches for a *moving revision*
/// scope (`ktmr diff -r HEAD`, the scope menu's "Revision…") possibly
/// pointing at a different commit — resolved once at watch-start via
/// [`GitSource::git_path`], never hand-joined onto the repository root (see
/// that method's docs and [`spawn_revision_watcher`]'s: a git *worktree*'s
/// `.git` is a file, not a directory, and per-worktree state lives under a
/// private gitdir elsewhere entirely).
enum RevisionWatchTarget {
    /// A single file — relevant only when a notify event names this exact
    /// path. `HEAD`/`logs/HEAD`/`packed-refs` all share a git-dir with
    /// `index`/`COMMIT_EDITMSG`/`ORIG_HEAD`/`FETCH_HEAD` and everything
    /// else a plain `git add` or `git commit` also touches there — an
    /// unfiltered whole-directory watch would fire a resolve-and-maybe-
    /// re-diff check on every single one of those, not just the ones that
    /// can actually mean a moving scope's target changed.
    Exact(PathBuf),
    /// A whole directory tree — every change under it is relevant. `refs/`
    /// and `logs/refs/` contain nothing *but* ref files and reflogs to
    /// begin with, so there's nothing to filter out from either.
    Prefix(PathBuf),
}

/// The git-dir-relative paths whose changes can mean a moving revision like
/// `HEAD` now names a different commit — see this module's docs and issue
/// #8's grounding: an amend on a branch rewrites that branch's ref under
/// `refs/`, `logs/refs/…`, and `logs/HEAD` but *not* `.git/HEAD` itself
/// (which stays a symbolic ref); a detached-HEAD amend rewrites `HEAD`
/// directly instead. `packed-refs` covers the (rarer, but real) case a ref
/// lives there rather than as a loose file under `refs/`. A colocated jj
/// repo needs nothing extra watched here — jj keeps its own git `HEAD`/refs
/// in sync on every operation (confirmed empirically while building this
/// feature: `jj describe`/`jj squash` rewrite the colocated `.git/HEAD`
/// exactly like a `git commit --amend` would) — see this module's docs on
/// why `.jj/` internals are deliberately never watched at all.
const REVISION_WATCH_TARGETS: &[(&str, bool)] = &[
    ("HEAD", false),
    ("logs/HEAD", false),
    ("refs", true),
    ("logs/refs", true),
    ("packed-refs", false),
];

/// Whether a raw (absolute) notify event path is one this module actually
/// cares about, given the resolved `targets` [`RevisionWatchSession::start`]
/// built at watch-start — the same "classify against concrete, already-
/// resolved paths, no I/O" shape [`is_excluded`] already has, and pure for
/// the same reason: fully unit-testable with hand-built [`PathBuf`]s, no
/// real git repository required.
fn is_revision_relevant(targets: &[RevisionWatchTarget], path: &Path) -> bool {
    targets.iter().any(|target| match target {
        RevisionWatchTarget::Exact(p) => p == path,
        RevisionWatchTarget::Prefix(p) => path.starts_with(p),
    })
}

/// Starts the issue #8 ref-watcher against `git`'s repository and sends `()`
/// on `tx` (debounced — see [`REVISION_DEBOUNCE`]) each time a watched
/// git-dir path changes. Sibling of [`spawn_comments_watcher`] in every way
/// that matters: synchronous setup so a caller learns immediately if it
/// failed, its own thread for the pipeline, and a bare `()` signal — `ui::mod`
/// decides what a tick actually means (re-resolve the current moving scope,
/// cheaply, and only re-diff if it actually changed — see
/// `handle_moving_scope_refresh`), the same way it decides what a comments
/// signal means.
///
/// Each [`REVISION_WATCH_TARGETS`] entry that [`GitSource::git_path`]
/// resolves to a path that doesn't exist on disk yet (a fresh repo's
/// `logs/HEAD` before its first commit, or `packed-refs` in a repo that has
/// never been packed) is skipped rather than failing the whole watcher —
/// `notify` can't watch a path that isn't there, and "one target doesn't
/// exist yet" is routine, not a setup failure the caller should hear about.
pub fn spawn_revision_watcher(git: &GitSource, tx: Sender<()>) -> notify::Result<()> {
    let session = RevisionWatchSession::start(git)?;
    std::thread::spawn(move || session.run(tx));
    Ok(())
}

/// As [`COMMENTS_DEBOUNCE`]: a ref update is a tiny, infrequent write, not
/// the "formatter rewrites a whole crate" burst [`DEBOUNCE_QUIET`] exists
/// for — just enough to collapse the handful of raw filesystem events one
/// `git commit --amend` can generate (it touches more than one of
/// [`REVISION_WATCH_TARGETS`] in a single operation) into one signal.
const REVISION_DEBOUNCE: Duration = Duration::from_millis(100);

struct RevisionWatchSession {
    targets: Vec<RevisionWatchTarget>,
    notify_rx: mpsc::Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher,
}

impl RevisionWatchSession {
    fn start(git: &GitSource) -> notify::Result<Self> {
        let (notify_tx, notify_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        })?;

        let mut targets = Vec::new();
        // Dedupes which directories actually get a `watch()` call: `HEAD`
        // and `packed-refs` both resolve to the same top-level git-dir, and
        // registering that twice would be redundant (harmless on most
        // backends, but needless all the same).
        let mut watched_dirs = std::collections::HashSet::new();

        for &(relative, is_dir) in REVISION_WATCH_TARGETS {
            let resolved = git.git_path(relative).map_err(|e| {
                notify::Error::generic(&format!("failed to resolve git-path for {relative}: {e}"))
            })?;
            if !resolved.exists() {
                continue; // skip, not fail — see this function's own docs
            }
            let (watch_dir, mode) = if is_dir {
                (resolved.clone(), RecursiveMode::Recursive)
            } else {
                let parent = resolved
                    .parent()
                    .expect("a git-path file target always has a parent dir")
                    .to_path_buf();
                (parent, RecursiveMode::NonRecursive)
            };
            if watched_dirs.insert((watch_dir.clone(), mode)) {
                watcher.watch(&watch_dir, mode)?;
            }
            let target = if is_dir {
                RevisionWatchTarget::Prefix(resolved)
            } else {
                RevisionWatchTarget::Exact(resolved)
            };
            targets.push(target);
        }

        Ok(Self {
            targets,
            notify_rx,
            _watcher: watcher,
        })
    }

    fn run(self, tx: Sender<()>) {
        let mut last_sent: Option<Instant> = None;
        let mut deferred = false;
        loop {
            match self.notify_rx.recv_timeout(POLL_TICK) {
                Ok(Ok(event)) => {
                    // `classify` narrows to Create/Modify/Remove the same
                    // way it already does for the working-tree watcher — a
                    // mere *read* of `HEAD` (`EventKind::Access`, which every
                    // `git`/`jj` invocation triggers, including katamari's
                    // own) is not a ref changing, and without this filter it
                    // starves out the real write event: any git command run
                    // shortly after this session starts (warm-up, the scope
                    // swap's own resolve) counts as "relevant" purely by
                    // path, keeps re-arming `REVISION_DEBOUNCE`, and can
                    // easily suppress an amend's genuine Modify/Rename that
                    // lands within that same window.
                    let relevant = classify(&event)
                        .iter()
                        .any(|(path, _)| is_revision_relevant(&self.targets, path));
                    if relevant {
                        deferred = true;
                    }
                }
                Ok(Err(_)) => {
                    // As in `WatchSession::run`: one bad event from the
                    // platform backend doesn't invalidate the session.
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
            // Same defer-then-flush shape as the comments watcher's, and
            // for the same reason it stopped being a drop-inside-the-window
            // throttle: a second amend whose ref write lands within
            // [`REVISION_DEBOUNCE`] of the first's tick is machine-drivable
            // (a scripted rebase/amend loop) and its re-diff must not wait
            // for an unrelated third ref change. The window still collapses
            // one amend's own burst of ref/reflog events into one signal.
            if deferred && last_sent.is_none_or(|t| t.elapsed() >= REVISION_DEBOUNCE) {
                last_sent = Some(Instant::now());
                deferred = false;
                if tx.send(()).is_err() {
                    return;
                }
            }
        }
    }
}

/// Builds the gitignore filter from `repo_root`'s top-level `.gitignore`
/// only — not the full per-directory hierarchy a real `git status` walk
/// would consult (see [`ignore::WalkBuilder`] for that, which this module
/// deliberately doesn't pull in). A missing or malformed `.gitignore` is
/// not an error here: [`Gitignore::new`] tolerates both, producing a
/// matcher that simply excludes nothing beyond [`HARDCODED_EXCLUDES`].
fn build_ignore(repo_root: &Path) -> Gitignore {
    Gitignore::new(repo_root.join(".gitignore")).0
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- spawn_revision_watcher (issue #8) ---------------------------------

    /// A real `git commit --amend` against a real repo, watched by a real
    /// `spawn_revision_watcher` session — the one thing the pure
    /// `is_revision_relevant_table` test above can't prove: that
    /// `git_path` resolution, `notify` registration, and the `classify`-based
    /// event-kind filter (see [`RevisionWatchSession::run`]'s docs) all
    /// actually wire together against a live filesystem, not just hand-built
    /// `PathBuf`s.
    #[test]
    fn spawn_revision_watcher_fires_on_a_real_amend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);

        let source = GitSource::at(path.to_path_buf());
        let (tx, rx) = mpsc::channel::<()>();
        spawn_revision_watcher(&source, tx).expect("failed to start revision watcher");

        std::fs::write(path.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["commit", "-q", "-a", "--amend", "--no-edit"]);

        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "expected a signal within 5s of a real amend"
        );
    }

    /// [`RevisionWatchSession`]'s side of the same defer-not-drop
    /// guarantee the comments test below pins: a second amend launched
    /// immediately after the first amend's signal was *received* lands its
    /// ref writes as deep inside [`REVISION_DEBOUNCE`]'s window as a test
    /// can deterministically get — under the old drop-inside-the-window
    /// throttle its re-diff waited for an unrelated later ref change,
    /// which is exactly how a scripted amend/rebase loop went stale. Same
    /// timing property as the comments test: a stall past the window only
    /// downgrades this to the leading-edge path, never a false failure.
    #[test]
    fn a_second_amend_inside_the_debounce_window_is_deferred_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();
        let git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(path)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {out:?}");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(path.join("a.txt"), "one\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "first"]);

        let source = GitSource::at(path.to_path_buf());
        let (tx, rx) = mpsc::channel::<()>();
        spawn_revision_watcher(&source, tx).expect("failed to start revision watcher");

        std::fs::write(path.join("a.txt"), "one\ntwo\n").unwrap();
        git(&["commit", "-q", "-a", "--amend", "--no-edit"]);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the first amend must forward on the leading edge"
        );

        // Distinct content, so this amend genuinely rewrites the ref
        // rather than risking a byte-identical commit git may not touch.
        std::fs::write(path.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["commit", "-q", "-a", "--amend", "--no-edit"]);
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "an amend landing inside the debounce window must still produce \
             a (deferred) signal, not go stale"
        );
    }

    /// The defer-not-drop half of the debounce (see
    /// [`CommentsWatchSession::run`]): a second write issued immediately
    /// after the first signal was *received* is as deep inside the
    /// debounce window as a test can deterministically get — under the old
    /// drop-inside-the-window throttle it was lost until some unrelated
    /// later write, which is exactly how a scripted `comments add` +
    /// `resolve` pair loses its `resolve`. Timing can only make this test
    /// weaker, never flaky-fail: if the machine stalls past the window
    /// between the two writes, the second forwards on the leading edge and
    /// the assertion still holds.
    #[test]
    fn a_comments_write_inside_the_debounce_window_is_deferred_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".katamari")).unwrap();
        let comments = root.join(".katamari/comments.jsonl");

        let (tx, rx) = mpsc::channel::<()>();
        spawn_comments_watcher(root.to_path_buf(), tx).expect("failed to start comments watcher");
        // The watcher registers with `notify` before `spawn_comments_watcher`
        // returns (registration happens in `start`, on this thread), so the
        // first write below cannot race the watch itself.

        std::fs::write(&comments, "{\"id\":\"one\"}\n").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the first write must forward on the leading edge"
        );

        std::fs::write(&comments, "{\"id\":\"one\"}\n{\"id\":\"two\"}\n").unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a write landing inside the debounce window must still produce \
             a (deferred) signal, not vanish"
        );
    }

    /// Regression test for the bug where `.katamari/.gitignore` never got
    /// written in the ordinary `ktmr diff` startup path: this watcher spawns
    /// unconditionally on every diff session, usually *before* any comment
    /// is ever appended, so if it created `.katamari/` with a raw
    /// `create_dir_all` (as it once did) instead of going through
    /// [`crate::comments::CommentStore::ensure_dir`], the directory would
    /// already exist by the time a comment write got around to it — and
    /// `ensure_dir`'s `is_new` gate (see its docs) means the `.gitignore`
    /// then never lands at all. Starting against a repo root with no
    /// `.katamari/` yet, as here, is exactly the "nobody has commented in
    /// this session" case the old bug shipped in.
    #[test]
    fn starting_on_a_fresh_repo_seeds_the_katamari_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!root.join(".katamari").exists());

        let (tx, _rx) = mpsc::channel::<()>();
        spawn_comments_watcher(root.to_path_buf(), tx).expect("failed to start comments watcher");

        assert_eq!(
            std::fs::read_to_string(root.join(".katamari").join(".gitignore")).unwrap(),
            "*\n"
        );
    }

    fn write(dir: &Path, relative: &str, contents: &str) {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn hardcoded_excludes_are_dropped_regardless_of_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let ignore = build_ignore(dir.path());
        for sub in [
            ".git/HEAD",
            "target/debug/build",
            "node_modules/pkg/index.js",
        ] {
            let path = dir.path().join(sub);
            assert!(
                is_excluded(dir.path(), &path, &ignore),
                "{sub} should be excluded"
            );
        }
    }

    #[test]
    fn gitignored_paths_are_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "*.log\nbuild/\n");
        let ignore = build_ignore(dir.path());

        assert!(is_excluded(
            dir.path(),
            &dir.path().join("debug.log"),
            &ignore
        ));
        assert!(is_excluded(
            dir.path(),
            &dir.path().join("build/output.txt"),
            &ignore
        ));
        assert!(!is_excluded(
            dir.path(),
            &dir.path().join("src/main.rs"),
            &ignore
        ));
    }

    #[test]
    fn tracked_source_files_are_not_excluded() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "*.log\n");
        let ignore = build_ignore(dir.path());
        let path = dir.path().join("src").join("main.rs");
        write(dir.path(), "src/main.rs", "fn main() {}");
        assert!(!is_excluded(dir.path(), &path, &ignore));
    }

    #[test]
    fn classify_maps_create_modify_remove_and_drops_everything_else() {
        let event = |kind: EventKind, path: &str| Event {
            kind,
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        };
        assert_eq!(
            classify(&event(
                EventKind::Create(notify::event::CreateKind::File),
                "a"
            )),
            vec![(PathBuf::from("a"), ChangeKind::Created)]
        );
        assert_eq!(
            classify(&event(
                EventKind::Modify(notify::event::ModifyKind::Data(
                    notify::event::DataChange::Any
                )),
                "b"
            )),
            vec![(PathBuf::from("b"), ChangeKind::Changed)]
        );
        assert_eq!(
            classify(&event(
                EventKind::Remove(notify::event::RemoveKind::File),
                "c"
            )),
            vec![(PathBuf::from("c"), ChangeKind::Deleted)]
        );
        assert!(classify(&event(EventKind::Any, "d")).is_empty());
    }

    // ---- is_revision_relevant (issue #8) -----------------------------------

    /// A table test against hand-built targets (no real `GitSource`/`git`
    /// process involved — see [`is_revision_relevant`]'s own docs on why
    /// this can be pure), covering every [`RevisionWatchTarget`] variant and
    /// the noisy-sibling-file case [`RevisionWatchTarget::Exact`] exists to
    /// filter out.
    #[test]
    fn is_revision_relevant_table() {
        let git_dir = PathBuf::from("/repo/.git");
        let targets = vec![
            RevisionWatchTarget::Exact(git_dir.join("HEAD")),
            RevisionWatchTarget::Exact(git_dir.join("packed-refs")),
            RevisionWatchTarget::Prefix(git_dir.join("refs")),
        ];

        // Exact matches.
        assert!(is_revision_relevant(&targets, &git_dir.join("HEAD")));
        assert!(is_revision_relevant(&targets, &git_dir.join("packed-refs")));
        // Prefix matches, including a nested path under the watched dir.
        assert!(is_revision_relevant(
            &targets,
            &git_dir.join("refs").join("heads").join("main")
        ));
        assert!(is_revision_relevant(&targets, &git_dir.join("refs")));

        // A noisy sibling in the same (non-recursively watched) git-dir
        // that isn't itself a watch target must NOT be treated as relevant
        // — this is the whole reason `Exact` exists rather than just
        // watching the git-dir wholesale and filtering nothing.
        assert!(!is_revision_relevant(&targets, &git_dir.join("index")));
        assert!(!is_revision_relevant(
            &targets,
            &git_dir.join("COMMIT_EDITMSG")
        ));
        // A directory this session never resolved a target under at all.
        assert!(!is_revision_relevant(
            &targets,
            &git_dir.join("logs").join("HEAD")
        ));
    }
}
