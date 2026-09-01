//! Watches a repository's working tree for changes and turns them into
//! debounced batches [`crate::ui`]'s event loop can refresh a diff review
//! from, without the reviewer ever pressing a key. Three concerns, each
//! kept separate: [`debounce`] owns purely the "when has this burst of
//! events settled" arithmetic (independently unit-testable, no clock or
//! thread of its own); this module owns turning raw `notify` events into
//! [`ChangedPath`]s (filtering out `.git/`, build output, and gitignored
//! paths along the way) and running that pipeline on its own thread; the
//! caller (`ui::mod`) owns what a flushed batch actually triggers.
//!
//! Registration (getting `notify` to actually watch the right set of
//! directories) is platform-split — see [`WatchSession::start`]'s call
//! into the cfg-gated `register` — because `notify`'s backend families fall
//! into two cost shapes, not one-per-platform: FSEvents (macOS) and
//! `ReadDirectoryChangesW` (Windows) each watch an entire subtree with one
//! kernel-level call and a native recursive flag, so a single
//! [`RecursiveMode::Recursive`] registration at `repo_root` is both
//! simplest and fastest on either; inotify (Linux and every other target
//! this crate builds for) has no subtree primitive at all — `notify`'s own
//! `Recursive` mode there is already just a walk-and-register-every-
//! directory loop done for you, gitignore-blind. On that family this
//! module does that same walk itself instead, with [`ignore::WalkBuilder`],
//! so a directory that's gitignored, hardcoded-excluded, or a nested
//! checkout (a linked worktree, a vendored repo) never gets a watch
//! descriptor in the first place, rather than being registered and then
//! filtered event-by-event forever after — though the walk still
//! *descends* into a plain gitignore-matched directory even when it won't
//! register one for it, because a `!`-negated pattern deeper inside (e.g.
//! `!build/keep/` under a wholesale `build/` exclude) can re-admit a
//! descendant that only a walk which actually reaches it can discover; see
//! [`walk_descends`] for the pruning rule this needs, distinct from
//! [`walk_admits`], the shared registration decision both that walk and
//! this module's per-event filtering ([`is_excluded`]) are built from.

pub mod debounce;

pub use debounce::{ChangeKind, ChangedPath};

use crate::vcs::git::GitSource;
use debounce::Debounce;
use ignore::Match;
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
use ignore::WalkBuilder;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
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

/// What the watcher thread sends: a flushed batch, a lightweight heads-up
/// that a debounce window just opened (the first change since the last
/// flush) — purely a UI hint so a status bar can show "something's
/// changing" before the batch it belongs to is ready, nothing downstream
/// treats it as anything more than that — or a setup failure (see
/// [`spawn`]'s docs for why that's a signal now rather than a return
/// value).
pub enum WatchSignal {
    Pending,
    Flushed(WatchBatch),
    /// Registration itself failed — `repo_root` doesn't exist, or the
    /// platform's file-watching backend couldn't be initialized at all.
    /// Delivered exactly once, in place of any `Pending`/`Flushed` signal
    /// this session will never send (the watcher thread exits right after
    /// sending this): `ui::mod`'s consumer treats it as "nothing is
    /// watching," not as one bad tick in an otherwise-live session.
    Failed(notify::Error),
}

/// Starts watching `repo_root` and sends [`WatchSignal`]s to `tx` until the
/// receiving end goes away — entirely on its own thread, setup included.
/// Registration (see the module docs on the macOS/inotify split) can mean
/// walking the whole working tree, which on a large or worktree-heavy repo
/// is the single most expensive thing a session start does; a caller must
/// never block its first real frame on that, so unlike this function's
/// previous shape (a synchronous `notify::Result` return, everything up to
/// `watcher.watch()` done on the calling thread before spawning anything)
/// setup now happens after the thread starts, and a setup failure surfaces
/// asynchronously as [`WatchSignal::Failed`] over the same channel instead
/// of a return value — `ui::mod::start_watch` used to learn this
/// synchronously; see its docs for the async equivalent.
///
/// `quiet` is the trailing-edge debounce window (config's `[watch]
/// debounce_ms`, default [`DEBOUNCE_QUIET`]) — how long a burst of changes
/// must go silent before it flushes. [`DEBOUNCE_MAX_LATENCY`]'s backstop is
/// not configurable: it exists to bound worst-case staleness under a
/// continuous stream of writes, a concern orthogonal to how snappy an
/// ordinary quiet-period flush feels, which is the only thing `quiet`
/// tunes.
pub fn spawn(repo_root: PathBuf, tx: Sender<WatchSignal>, quiet: Duration) {
    std::thread::spawn(move || {
        let session = match WatchSession::start(repo_root) {
            Ok(session) => session,
            Err(e) => {
                let _ = tx.send(WatchSignal::Failed(e));
                return;
            }
        };
        // Closes the async-setup race registration now opens: anything
        // that changed on disk between process start and registration
        // finishing (above) would otherwise never trigger a refresh at
        // all — no watch was active yet to see it happen, so nothing
        // downstream of `notify` would ever hear about it. One guaranteed
        // catch-up flush, through the ordinary `Flushed` path rather than
        // a dedicated signal: `handle_watch_refresh` already treats an
        // empty `changes` batch as a legitimate input (just re-runs `git
        // diff`), and for the common case — nothing actually raced the
        // registration window — that re-derives a byte-identical diff, a
        // harmless no-op refresh. Sent before `run` so it can never be
        // reordered behind a real early batch; `run`'s own debounce
        // window absorbs anything that lands in the same instant.
        if tx
            .send(WatchSignal::Flushed(WatchBatch {
                changes: Vec::new(),
            }))
            .is_err()
        {
            return;
        }
        session.run(tx, quiet);
    });
}

/// One watcher connection: the underlying OS watcher, the gitignore filter
/// built once at start, and the debounce state that accumulates across the
/// session. `watcher` stays live for more than its `Drop` on the filtered
/// (inotify-family) registration strategy — [`WatchSession::maybe_register_new_dir`]
/// calls `.watch()` on it again whenever a directory appears mid-session;
/// on the recursive strategy (macOS and Windows both — see the module
/// docs) nothing ever calls it again after [`WatchSession::start`], hence
/// the lint escape hatch. `registered_dirs` exists only for the filtered
/// strategy: every directory [`register_subtree`]'s walk has already
/// visited this session, watched or not (see that function's docs), so a
/// later event on a directory already in this set short-circuits before
/// re-walking its subtree — without it, widening
/// [`WatchSession::maybe_register_new_dir`]'s trigger to any directory
/// event rather than just `Created` (needed so a directory renamed/moved
/// in is ever picked up at all) would turn every ordinary metadata-only
/// `Modify` on an already-watched directory into a full re-walk of it.
struct WatchSession {
    repo_root: PathBuf,
    ignore: Gitignore,
    notify_rx: mpsc::Receiver<notify::Result<Event>>,
    #[cfg_attr(any(target_os = "macos", target_os = "windows"), allow(dead_code))]
    watcher: RecommendedWatcher,
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    registered_dirs: std::collections::HashSet<PathBuf>,
}

impl WatchSession {
    fn start(repo_root: PathBuf) -> notify::Result<Self> {
        let ignore = build_ignore(&repo_root);
        let (notify_tx, notify_rx) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(move |res| {
            let _ = notify_tx.send(res);
        })?;
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            register(&repo_root, &ignore, &mut watcher)?;
            Ok(Self {
                repo_root,
                ignore,
                notify_rx,
                watcher,
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            // `register`'s return value seeds `registered_dirs` with every
            // directory the initial walk already visited, so a later event
            // on one of them (an ordinary mtime bump, say) doesn't look
            // "new" to `maybe_register_new_dir` and trigger a redundant
            // re-walk — see that field's docs.
            let registered_dirs = register(&repo_root, &ignore, &mut watcher)?;
            Ok(Self {
                repo_root,
                ignore,
                notify_rx,
                watcher,
                registered_dirs,
            })
        }
    }

    /// Runs the classify → filter → debounce pipeline until `tx`'s receiver
    /// is dropped (the normal way this ends: the TUI session exited) or the
    /// underlying `notify` channel disconnects (the platform watcher itself
    /// died, which nothing currently recovers from — a watch session that
    /// silently stopped noticing changes would be worse than one that's
    /// visibly gone). Takes `self` by value now that it's the *only* thing
    /// still running on this thread (registration used to happen before it,
    /// on the caller's thread — see [`spawn`]'s docs), and by `mut`:
    /// `maybe_register_new_dir` needs `&mut self.watcher` on the filtered
    /// registration strategy.
    fn run(mut self, tx: Sender<WatchSignal>, quiet: Duration) {
        let start = Instant::now();
        let mut debounce = Debounce::new(quiet, DEBOUNCE_MAX_LATENCY);

        loop {
            match self.notify_rx.recv_timeout(POLL_TICK) {
                Ok(Ok(event)) => {
                    for (path, kind) in classify(&event) {
                        if is_excluded(&self.repo_root, &path, &self.ignore) {
                            continue;
                        }
                        // Captured before either record below, so a new
                        // directory's dynamic-registration catch-up (which
                        // records into `debounce` itself — see that
                        // method's docs) can never steal the "this is the
                        // window-opening event" `Pending` hint out from
                        // under the directory's own `Created` record that
                        // follows it.
                        let opening_window = debounce.is_empty();
                        self.maybe_register_new_dir(&path, kind, &mut debounce, start);
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

    /// On the filtered (inotify-family) registration strategy, notices a
    /// directory this session hasn't already registered — a brand-new one
    /// (`Created`), or one that just moved into the tree via `mv`/`git mv`
    /// (on inotify, `notify` reports that as `Modify(Name(RenameMode::
    /// To/Both))`, which [`classify`] maps to `Changed` — a directory
    /// arriving this way used to be permanently unwatched, since the old
    /// `kind == Created` gate never called this for it at all, and no
    /// OS-level watch descriptor was ever placed on it as a result) — and
    /// gives it (and everything admitted beneath it — a burst-created
    /// subtree, e.g. `mkdir -p a/b/c`, can bring more than one directory
    /// into existence before this session reacts at all) its own watch,
    /// the same way [`register`] did for every directory that already
    /// existed at startup. A no-op on the recursive strategy (macOS and
    /// Windows both): their native subtree watch already covers new or
    /// renamed-in subdirectories on its own — nothing to register.
    ///
    /// `path.is_dir()` alone gates this now — `kind` isn't consulted for
    /// the trigger any more (a deleted path fails `is_dir()` on its own, so
    /// no separate exclusion is needed for that case) — re-stat rather than
    /// trusting `notify`'s `CreateKind`/`ModifyKind` (which some backends
    /// report as an uninformative `Any`), matching how [`is_excluded`]
    /// already re-stats for the same reason. `registered_dirs` (see that
    /// field's docs) then gates the actual work: a directory already in the
    /// set is skipped outright before touching the filesystem again, so a
    /// routine `Modify` on a directory this session registered minutes ago
    /// (a child being added bumps the parent's mtime) costs one `HashSet`
    /// lookup, not a re-walk.
    ///
    /// The walk-or-not decision for a not-yet-seen directory is
    /// [`walk_descends`], not [`walk_admits`] — deliberately more
    /// permissive: a directory that's itself gitignored (a `build/` just
    /// `mv`'d in, say) still gets walked, so a negated descendant inside it
    /// (`build/keep/`) can still be found and registered even though
    /// `build/` itself won't be (see the module docs and
    /// [`walk_descends`]'s). Only a hardcoded exclude or a nested checkout
    /// (a newly arrived worktree or vendored repo) skips the walk
    /// entirely; [`register_subtree`]'s own [`walk_admits`] check then
    /// decides, directory by directory, which of what the walk finds
    /// actually gets a watch. Best-effort either way (see
    /// [`register_subtree`]'s docs on why a failure here doesn't tear down
    /// the session): it closes a second, narrower async-setup-shaped race
    /// than [`spawn`]'s startup one — any write that lands inside the new
    /// directory between the OS creating it and the `watcher.watch()` call
    /// landing is invisible to `notify` forever, since no watch was
    /// covering that path yet to generate an event for it. Recorded into
    /// `debounce` as a synthetic `Changed` entry (not sent immediately) so
    /// it collapses into the same flush as every other event in the
    /// current window instead of forcing an extra, undebounced refresh —
    /// the directory's own record (added by the caller right after this
    /// returns) would otherwise already trigger a refresh on its own, but
    /// that alone wouldn't prove anything written *inside* it was actually
    /// seen.
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn maybe_register_new_dir(
        &mut self,
        _path: &Path,
        _kind: ChangeKind,
        _debounce: &mut Debounce,
        _start: Instant,
    ) {
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    fn maybe_register_new_dir(
        &mut self,
        path: &Path,
        _kind: ChangeKind,
        debounce: &mut Debounce,
        start: Instant,
    ) {
        if !path.is_dir() {
            return;
        }
        if !walk_descends(&self.repo_root, path) {
            return; // hardcoded-excluded, or a nested checkout — see `walk_descends`
        }
        if self.registered_dirs.contains(path) {
            return; // already registered/walked this session
        }
        if register_subtree(
            &self.repo_root,
            path,
            &self.ignore,
            &mut self.watcher,
            &mut self.registered_dirs,
        )
        .is_ok()
        {
            debounce.record(start.elapsed(), path.to_path_buf(), ChangeKind::Changed);
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

/// The [`HARDCODED_EXCLUDES`] half of [`is_excluded`], factored out so
/// [`walk_descends`] can prune on it too without pulling in the
/// `.gitignore` check alongside it — unlike a hardcoded exclude, a
/// gitignore match admits negation and so must never prune walk *descent*
/// on its own (see that function's docs and the module docs' Finding-#2
/// note). Same fail-open rule for a `path` outside `repo_root` as
/// `is_excluded`.
fn is_hardcoded_excluded(repo_root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(repo_root) else {
        return false;
    };
    relative.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|name| HARDCODED_EXCLUDES.contains(&name))
    })
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
    if is_hardcoded_excluded(repo_root, path) {
        return true;
    }
    let Ok(relative) = path.strip_prefix(repo_root) else {
        return false;
    };
    let is_dir = path.is_dir();
    matches!(
        ignore.matched_path_or_any_parents(relative, is_dir),
        Match::Ignore(_)
    )
}

/// Registers `repo_root` with `watcher` — see the module docs for why this
/// is platform-split. The macOS/FSEvents and Windows/`ReadDirectoryChangesW`
/// strategy: subtree watching is a single native kernel-level call on
/// either backend (FSEvents recurses by construction; `notify`'s Windows
/// backend passes `RecursiveMode::Recursive` straight through as
/// `ReadDirectoryChangesW`'s own `bWatchSubtree` flag), so one recursive
/// registration at `repo_root` is both the simplest and the fastest option
/// on both platforms; walking the tree ourselves the way the inotify-family
/// strategy does would only add per-directory `watch()` call overhead
/// neither backend needs and gain nothing (no ignore-awareness benefit
/// either, since neither has a per-directory registration to skip in the
/// first place).
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn register(
    repo_root: &Path,
    _ignore: &Gitignore,
    watcher: &mut RecommendedWatcher,
) -> notify::Result<()> {
    watcher.watch(repo_root, RecursiveMode::Recursive)
}

/// Registers `repo_root` with `watcher` on inotify-family platforms (every
/// target other than macOS/Windows — see the module docs): a non-recursive
/// watch per ignore-admitted directory, walked and filtered by
/// [`walk_admits`]/[`walk_descends`] (see the module docs for the full
/// rationale). This costs no *more* watch descriptors than `notify`'s own
/// `RecursiveMode::Recursive` did before this change — on this backend
/// family, `notify` already registers one inotify watch per directory
/// under the hood to emulate recursion, since inotify itself has no
/// subtree primitive; doing that walk ourselves instead just means some of
/// those directories (the gitignored, hardcoded-excluded, and
/// nested-checkout ones) never get a descriptor at all. Strictly fewer,
/// never more — nothing here should raise "watch limit" concerns.
///
/// Returns every directory the registration walk visited, watched or not
/// (see [`register_subtree`]) — the caller seeds
/// [`WatchSession`]'s `registered_dirs` with it, so a later event on one of
/// them can't trigger a redundant re-walk of the whole subtree.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn register(
    repo_root: &Path,
    ignore: &Gitignore,
    watcher: &mut RecommendedWatcher,
) -> notify::Result<std::collections::HashSet<PathBuf>> {
    let mut registered_dirs = std::collections::HashSet::new();
    register_subtree(repo_root, repo_root, ignore, watcher, &mut registered_dirs)?;
    Ok(registered_dirs)
}

/// Whether the filtered registration walk should descend into `path` at
/// all — independent of whether `path` itself ends up with its own watch
/// (that's [`walk_admits`], below). Only two reasons ever prune descent
/// here, and both share the property that no `.gitignore` negation could
/// ever reverse them: a [`HARDCODED_EXCLUDES`] name (not a gitignore
/// concept at all — [`is_excluded`] never lets a pattern override these
/// either) and a nested checkout's own `.git` boundary (a different
/// repository entirely, not this one's ignore rules to interpret). A plain
/// gitignore-matched directory is deliberately *not* pruned here — see the
/// module docs and [`walk_admits`]'s: a directory-level exclude alone must
/// never stop the walk from reaching a `!`-negated descendant, or that
/// negation becomes unreachable regardless of how correctly `walk_admits`
/// would answer for the descendant directly.
fn walk_descends(repo_root: &Path, path: &Path) -> bool {
    if path == repo_root {
        return true;
    }
    if is_hardcoded_excluded(repo_root, path) {
        return false;
    }
    !path.join(".git").exists()
}

/// Whether `path` should get its own `notify` watch — used by
/// [`register_subtree`] to decide, directory by directory, which of what
/// [`walk_watchable_dirs`] visits actually gets a `watcher.watch()` call.
/// Distinct from — and stricter than — [`walk_descends`], which decides
/// whether the walk visits `path` at all: a plain gitignore-matched
/// directory fails `walk_admits` (no watch of its own) but still passes
/// `walk_descends` (still visited), because a `!`-negated pattern deeper
/// inside it can re-admit a descendant that only a walk which reaches it
/// can discover. Before that split existed, the two questions shared one
/// verdict, which was exactly the bug: pruning descent the moment a
/// directory itself matched meant a negated child could never be reached
/// to ask about, no matter how correctly this function would have answered
/// for it.
///
/// `repo_root` itself is always kept regardless of what `ignore` says
/// about it (an empty or absent `.gitignore` must never make the walk
/// refuse its own starting point). Every other directory is refused a
/// watch for one of two reasons:
///
/// - [`is_excluded`] the same way an individual changed *file* there would
///   be (hardcoded excludes, gitignored) — the one decision this function
///   and event-time filtering both consult, so registration and the
///   debounce pipeline's own filter can't independently drift into
///   disagreeing about the same directory.
/// - it directly contains a `.git` entry (a file — a linked worktree's
///   `.git` always is one — or a directory) and isn't `repo_root` itself:
///   a *nested checkout*. Everything beneath it (an agent worktree under
///   `.claude/worktree/*`, a vendored/submodule-style checkout) belongs to
///   a different repository than the one this session is reviewing, and
///   `is_excluded` has no way to know that on its own (it only ever sees
///   one path at a time, never "does this directory's own listing contain
///   a `.git`").
fn walk_admits(repo_root: &Path, path: &Path, ignore: &Gitignore) -> bool {
    if path == repo_root {
        return true;
    }
    if is_excluded(repo_root, path, ignore) {
        return false;
    }
    !path.join(".git").exists()
}

/// Walks `walk_root`'s subtree collecting every directory the filtered
/// registration strategy *visits* — not just the ones that end up with a
/// watch (that filter is [`register_subtree`]'s, via [`walk_admits`], on
/// what this returns); `walk_root` itself is excluded from the result:
/// both call sites ([`register`] with `walk_root = repo_root`, and dynamic
/// re-registration via [`register_subtree`] with `walk_root` = a
/// just-created or just-renamed-in directory) handle that one directly,
/// unconditionally, before calling this.
///
/// Pruned only by [`walk_descends`] — a hardcoded exclude or a nested
/// checkout — deliberately *not* by [`walk_admits`]'s fuller gitignore
/// check: a plain gitignore-matched directory (`build/`) is still walked
/// into, so a `!`-negated descendant (`build/keep/`) is still found and
/// handed to the caller to register, even though `build/` itself won't be.
/// See the module docs and [`walk_descends`]'s for why collapsing that
/// distinction (as this function used to) silently breaks gitignore
/// negation.
///
/// `repo_root` is threaded through separately from `walk_root` because
/// [`walk_descends`] needs the repository's *true* root to compute a
/// hardcoded-exclude-relative path correctly, even when `walk_root` is
/// some deeper directory that just appeared mid-session.
///
/// Built on [`ignore::WalkBuilder`] purely as a directory walker: all of
/// its own gitignore/hidden-file machinery is switched off
/// (`standard_filters(false)`) so [`walk_descends`] is the only thing
/// deciding what gets pruned here, rather than layering a second,
/// independently-configured ignore engine that could quietly disagree with
/// [`is_excluded`]/[`walk_admits`]. `hidden` in particular must stay off: a
/// dotdir like `.github/` is ordinary, watched content today (nothing
/// hardcoded-excludes it, nothing gitignores it by default) —
/// `WalkBuilder`'s own default would otherwise silently stop watching it.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn walk_watchable_dirs(repo_root: &Path, walk_root: &Path) -> Vec<PathBuf> {
    let mut builder = WalkBuilder::new(walk_root);
    builder.standard_filters(false).hidden(false);
    let root = repo_root.to_path_buf();
    builder.filter_entry(move |entry| walk_descends(&root, entry.path()));
    builder
        .build()
        .filter_map(Result::ok)
        .filter(|entry| entry.path() != walk_root)
        .filter(|entry| entry.file_type().is_some_and(|ft| ft.is_dir()))
        .map(ignore::DirEntry::into_path)
        .collect()
}

/// Walks everything [`walk_watchable_dirs`] visits beneath `dir` (`dir`
/// itself included) and gives each directory [`walk_admits`] approves its
/// own non-recursive watch — used both for the initial registration walk
/// ([`register`], `dir = repo_root`) and for dynamic re-registration when a
/// directory appears mid-session (`WatchSession::maybe_register_new_dir`,
/// `dir` = the new or renamed-in directory): the exact same "watch this
/// subtree, ignore-aware" operation either way, just rooted somewhere other
/// than the repository root the second time.
///
/// `seen` collects every directory the walk visited, watched or not (see
/// [`walk_watchable_dirs`]'s docs on why a gitignore-matched-but-not-
/// hardcoded directory is still visited without being watched), so the
/// caller can fold it into [`WatchSession`]'s own registered-directories
/// tracking and never pay to walk the same subtree twice.
///
/// `dir` itself is registered only when [`walk_admits`] approves it —
/// always true for `repo_root` (see that function's docs), but not
/// necessarily for a directory arriving mid-session: one might itself be a
/// gitignore match (a `build/` just `mv`'d in from outside), which still
/// needs walking for negated descendants but must not get a watch of its
/// own — the Finding-#2 distinction `walk_admits` vs. `walk_descends`
/// exists to make. Everything beneath `dir` is best-effort regardless
/// (`let _ = watcher.watch(...)`, one directory at a time, still gated by
/// `walk_admits`): a descendant that vanished between being walked and
/// being registered (a real, if narrow, race) or one this process can't
/// read for permission reasons shouldn't take the whole registration down,
/// the same "a narrower failure mode beats a session-ending one" call
/// [`is_excluded`]'s own fail-open behavior makes. Only `dir`'s own
/// `watch()` call (when attempted) can fail this function at all: for the
/// initial call `dir` is `repo_root`, and a session that can't watch its
/// own repository root has nothing left to watch at all, so that failure
/// must propagate.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn register_subtree(
    repo_root: &Path,
    dir: &Path,
    ignore: &Gitignore,
    watcher: &mut RecommendedWatcher,
    seen: &mut std::collections::HashSet<PathBuf>,
) -> notify::Result<()> {
    seen.insert(dir.to_path_buf());
    if walk_admits(repo_root, dir, ignore) {
        watcher.watch(dir, RecursiveMode::NonRecursive)?;
    }
    for path in walk_watchable_dirs(repo_root, dir) {
        seen.insert(path.clone());
        if walk_admits(repo_root, &path, ignore) {
            let _ = watcher.watch(&path, RecursiveMode::NonRecursive);
        }
    }
    Ok(())
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

/// Watches `<repo_root>/.katamari/` for changes to `reviewed.jsonl` and
/// sends a signal on `tx` each time it does — the concurrent-session
/// counterpart to [`spawn_comments_watcher`], added after a reviewer caught
/// live that two `ktmr diff` sessions open on the same repository (e.g. one
/// on the working tree, one on `--pr N`) never converged on each other's
/// reviewed marks without a restart, even though the reviewed-hunk feature's
/// own design point is marks "shared across every scope pointing at the
/// same repo." Deliberately its own session against the same `.katamari/`
/// directory `spawn_comments_watcher` already watches, rather than teaching
/// that session to watch two filenames and dispatch by which one fired: the
/// two signals mean different things downstream (a comments reload never
/// touches `App::rows`' shape; a reviewed reload always does, since it
/// re-runs the collapse pass — see [`crate::ui::app::App::reload_reviewed`]),
/// so keeping them as distinct channel variants the caller already branches
/// on outweighs the cost of one extra `notify` registration on a directory
/// that's already cheap to watch (a handful of small JSONL files, not a
/// working tree).
pub fn spawn_reviewed_watcher(repo_root: PathBuf, tx: Sender<()>) -> notify::Result<()> {
    let session = ReviewedWatchSession::start(repo_root)?;
    std::thread::spawn(move || session.run(tx));
    Ok(())
}

struct ReviewedWatchSession {
    reviewed_path: PathBuf,
    notify_rx: mpsc::Receiver<notify::Result<Event>>,
    _watcher: RecommendedWatcher,
}

impl ReviewedWatchSession {
    fn start(repo_root: PathBuf) -> notify::Result<Self> {
        // Mirrors `CommentsWatchSession::start`: the watched directory and
        // target filename come from `ReviewedStore::path` rather than being
        // hardcoded again here, for the same "one place owns the on-disk
        // layout decision" reason.
        let store = crate::reviewed::store::ReviewedStore::new(&repo_root);
        let reviewed_path = store.path().to_path_buf();
        let dir = reviewed_path
            .parent()
            .expect("reviewed.jsonl always has a parent")
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
            reviewed_path,
            notify_rx,
            _watcher: watcher,
        })
    }

    /// As [`CommentsWatchSession::run`] — same debounce/defer-not-drop
    /// shape, reusing [`COMMENTS_DEBOUNCE`] rather than a near-duplicate
    /// constant, since a reviewed-hunk mark is exactly the same "tiny,
    /// infrequent write" cost class a comment-log append is.
    fn run(self, tx: Sender<()>) {
        let mut last_sent: Option<Instant> = None;
        let mut deferred = false;
        let target_name = self.reviewed_path.file_name();
        loop {
            match self.notify_rx.recv_timeout(POLL_TICK) {
                Ok(Ok(event)) => {
                    let touches_reviewed =
                        matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_))
                            && event.paths.iter().any(|p| p.file_name() == target_name);
                    if touches_reviewed {
                        deferred = true;
                    }
                }
                Ok(Err(_)) => {}
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
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

/// Builds the single ignore-pattern matcher both event-time filtering
/// ([`is_excluded`]) and the filtered registration walk ([`walk_admits`])
/// share — the "derive both from one place" half of keeping registration
/// and event-filtering from disagreeing about the same path (the other
/// half, the nested-checkout rule, isn't a gitignore pattern at all and
/// lives in `walk_admits` itself). Three sources, same trio `git status`
/// itself consults, folded into one [`GitignoreBuilder`]: `repo_root`'s own
/// top-level `.gitignore`, `.git/info/exclude` (a second git-native source
/// that never lives in `.gitignore` — a personal/local exclusion an
/// engineer doesn't want to commit), and the global excludes file
/// (`core.excludesFile`, or the XDG-default fallback — resolved the exact
/// same way `git` itself would via [`ignore::gitignore::gitconfig_excludes_path`]).
///
/// Deliberately *not* the full per-directory hierarchy a real `git status`
/// walk consults (nested `.gitignore` files below `repo_root`) — that would
/// need re-deriving this matcher at every directory level rather than once
/// per session; see [`ignore::WalkBuilder`] for the crate's own machinery
/// that does do that, which the filtered registration walk deliberately
/// doesn't lean on for its *ignore* decisions (only as a walker — see
/// `walk_watchable_dirs`'s docs) for exactly this reason: two independently
/// full-hierarchy-aware engines (one for the walk, a separate one for
/// events) would be two places to keep in sync, not one. The gap this
/// leaves is pre-existing and narrow — a file un-ignored by a *nested*
/// `.gitignore` after being matched by a broader top-level pattern is
/// treated as still-ignored — and was already true of every path this
/// module has ever filtered, watch registration included, long before this
/// function grew the two extra sources.
///
/// A missing or malformed source is not an error for any of the three:
/// most repositories define neither `.git/info/exclude` content nor a
/// global excludes file, and [`GitignoreBuilder::add`] already tolerates a
/// missing/malformed `.gitignore` the same way the `Gitignore::new` call
/// this replaces did.
fn build_ignore(repo_root: &Path) -> Gitignore {
    let mut builder = GitignoreBuilder::new(repo_root);
    let _ = builder.add(repo_root.join(".gitignore"));
    let _ = builder.add(repo_root.join(".git").join("info").join("exclude"));
    if let Some(global) = ignore::gitignore::gitconfig_excludes_path() {
        let _ = builder.add(global);
    }
    builder.build().unwrap_or_else(|_| Gitignore::empty())
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

    /// Regression coverage for the concurrent-session bug a reviewer caught
    /// live: two `ktmr diff` sessions open on the same repo never converged
    /// on each other's reviewed marks because nothing watched
    /// `reviewed.jsonl` at all. Mirrors
    /// [`a_comments_write_inside_the_debounce_window_is_deferred_not_dropped`]
    /// exactly, against [`spawn_reviewed_watcher`] instead.
    #[test]
    fn a_reviewed_write_inside_the_debounce_window_is_deferred_not_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".katamari")).unwrap();
        let reviewed = root.join(".katamari/reviewed.jsonl");

        let (tx, rx) = mpsc::channel::<()>();
        spawn_reviewed_watcher(root.to_path_buf(), tx).expect("failed to start reviewed watcher");

        std::fs::write(
            &reviewed,
            "{\"hunk_id\":\"one\",\"path\":\"a\",\"marked_at\":0}\n",
        )
        .unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "the first write must forward on the leading edge"
        );

        std::fs::write(
            &reviewed,
            "{\"hunk_id\":\"one\",\"path\":\"a\",\"marked_at\":0}\n\
             {\"hunk_id\":\"two\",\"path\":\"b\",\"marked_at\":0}\n",
        )
        .unwrap();
        assert!(
            rx.recv_timeout(Duration::from_secs(5)).is_ok(),
            "a write landing inside the debounce window must still produce \
             a (deferred) signal, not vanish"
        );
    }

    /// A change to `comments.jsonl` alone must never wake the reviewed
    /// watcher (and, by the same filename-filtered logic in the other
    /// direction, a `reviewed.jsonl` write must never wake the comments
    /// watcher — both sessions share one directory registration but filter
    /// by target filename, see each session's own `run` docs).
    #[test]
    fn a_reviewed_watcher_ignores_writes_to_comments_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".katamari")).unwrap();

        let (tx, rx) = mpsc::channel::<()>();
        spawn_reviewed_watcher(root.to_path_buf(), tx).expect("failed to start reviewed watcher");

        std::fs::write(root.join(".katamari/comments.jsonl"), "{\"id\":\"one\"}\n").unwrap();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(500)),
            Err(RecvTimeoutError::Timeout),
            "a comments.jsonl write must not trigger a reviewed signal"
        );
    }

    #[test]
    fn reviewed_watcher_starting_on_a_fresh_repo_seeds_the_katamari_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(!root.join(".katamari").exists());

        let (tx, _rx) = mpsc::channel::<()>();
        spawn_reviewed_watcher(root.to_path_buf(), tx).expect("failed to start reviewed watcher");

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

    /// `build_ignore`'s second source ([`GitignoreBuilder`]'s docs): a
    /// pattern that lives in `.git/info/exclude` rather than a tracked
    /// `.gitignore` (the common reason to use it — a personal exclusion
    /// nobody else's checkout should see) must still exclude.
    #[test]
    fn git_info_exclude_patterns_are_respected() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".git/info/exclude", "*.local\n");
        let ignore = build_ignore(dir.path());
        assert!(is_excluded(
            dir.path(),
            &dir.path().join("scratch.local"),
            &ignore
        ));
    }

    /// Finding #2's grounding primitive, isolated from the registration
    /// walk entirely: `Gitignore::matched_path_or_any_parents` resolves a
    /// `!`-negated pattern nested under a wholesale directory exclude by
    /// checking the full path directly, not by refusing to look past an
    /// excluded parent the way a real directory-pruning walk would — so
    /// `build/` itself stays excluded while `build/keep/` (and everything
    /// under it) does not. `walk_descends`/`walk_admits`'s split exists
    /// entirely so the registration walk can actually reach this correct
    /// answer for `build/keep` instead of pruning past it at `build/`.
    #[test]
    fn gitignore_negation_re_admits_a_nested_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "build/\n!build/keep/\n");
        write(dir.path(), "build/keep/file.txt", "content\n");
        let ignore = build_ignore(dir.path());
        assert!(is_excluded(dir.path(), &dir.path().join("build"), &ignore));
        assert!(!is_excluded(
            dir.path(),
            &dir.path().join("build/keep/file.txt"),
            &ignore
        ));
    }

    // ---- walk_descends / walk_admits (filtered registration, inotify-family only) --

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_admits_keeps_the_repo_root_regardless_of_ignore_content() {
        let dir = tempfile::tempdir().unwrap();
        // A pattern that would (if it could even apply to the root's own
        // empty relative path) claim to exclude everything — the
        // `path == repo_root` fast path must win regardless.
        write(dir.path(), ".gitignore", "*\n");
        let ignore = build_ignore(dir.path());
        assert!(walk_admits(dir.path(), dir.path(), &ignore));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_admits_skips_a_gitignored_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "build/\n");
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        let ignore = build_ignore(dir.path());
        assert!(!walk_admits(dir.path(), &dir.path().join("build"), &ignore));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_admits_skips_a_nested_checkout_with_a_dot_git_file() {
        let dir = tempfile::tempdir().unwrap();
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        // A linked worktree's `.git` is always a plain file (`gitdir:
        // .../ points elsewhere`), never a directory — this is the more
        // common real-world shape (`.claude/worktree/*`-style agent
        // worktrees) and must be caught the same as the directory form
        // below.
        std::fs::write(worktree.join(".git"), "gitdir: /somewhere/else\n").unwrap();
        let ignore = build_ignore(dir.path());
        assert!(!walk_admits(dir.path(), &worktree, &ignore));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_admits_skips_a_nested_checkout_with_a_dot_git_directory() {
        let dir = tempfile::tempdir().unwrap();
        let vendored = dir.path().join("vendored-repo");
        std::fs::create_dir_all(vendored.join(".git")).unwrap();
        let ignore = build_ignore(dir.path());
        assert!(!walk_admits(dir.path(), &vendored, &ignore));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_admits_keeps_an_ordinary_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let ignore = build_ignore(dir.path());
        assert!(walk_admits(dir.path(), &src, &ignore));
    }

    /// Finding #2, at the `walk_descends` level directly: a plain
    /// gitignore-matched directory must still be descended into (contrast
    /// with `walk_admits_skips_a_gitignored_directory` above, which is
    /// correctly false for the same directory) — otherwise a walk pruning
    /// on `walk_descends` alone would never reach `build/keep/` to
    /// register it.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_descends_does_not_prune_a_plain_gitignored_directory() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "build/\n");
        std::fs::create_dir_all(dir.path().join("build")).unwrap();
        assert!(walk_descends(dir.path(), &dir.path().join("build")));
    }

    /// `walk_descends`'s two real pruning reasons — a hardcoded exclude and
    /// a nested checkout — neither of which is a gitignore concept, so
    /// neither should ever be reachable by negation the way a plain
    /// gitignore match is.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_descends_skips_hardcoded_excludes_and_nested_checkouts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        let worktree = dir.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /elsewhere\n").unwrap();

        assert!(!walk_descends(dir.path(), &dir.path().join("target")));
        assert!(!walk_descends(dir.path(), &worktree));
        assert!(walk_descends(dir.path(), dir.path()));
    }

    /// End-to-end proof of Finding #2's fix at the walk level: given the
    /// exact `build/\n!build/keep/\n` shape the finding reproduced against
    /// a real compiled binary, the registration walk must still *visit*
    /// `build/keep` (via `walk_watchable_dirs`, pruned only by
    /// `walk_descends`) even though `build` itself correctly fails
    /// `walk_admits` and so never gets a watch of its own — proving the
    /// two-question split actually closes the gap, not just that each
    /// question answers correctly in isolation.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn walk_watchable_dirs_still_reaches_a_negated_descendant() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "build/\n!build/keep/\n");
        std::fs::create_dir_all(dir.path().join("build/keep")).unwrap();
        std::fs::write(dir.path().join("build/keep/file.txt"), "content\n").unwrap();
        let ignore = build_ignore(dir.path());

        let visited = walk_watchable_dirs(dir.path(), dir.path());
        assert!(
            visited.contains(&dir.path().join("build")),
            "the walk must still visit build/ to reach its negated child"
        );
        assert!(
            visited.contains(&dir.path().join("build/keep")),
            "the negated descendant must be visited: {visited:?}"
        );

        assert!(!walk_admits(dir.path(), &dir.path().join("build"), &ignore));
        assert!(walk_admits(
            dir.path(),
            &dir.path().join("build/keep"),
            &ignore
        ));
    }

    // ---- WatchSession::maybe_register_new_dir (Finding #1) ----------------

    /// Finding #1's real-`notify` reproduction: a directory that arrives by
    /// `mv` (not `mkdir`) generates a `Modify(Name(RenameMode::To))` event
    /// on inotify — `classify` maps that to `Changed`, not `Created` — so
    /// the old `kind == Created` gate never even looked at it, and it never
    /// got a watch descriptor of its own for the rest of the session.
    /// Mirrors `spawn_revision_watcher_fires_on_a_real_amend`'s "drive the
    /// real backend, not a hand-built `Event`" shape, since this is exactly
    /// the kind of platform-event-mapping detail a pure unit test over
    /// hand-built `Event`s can't prove.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn a_directory_renamed_into_the_tree_is_dynamically_watched() {
        // Blocks until a `Flushed` signal arrives (or the 5s deadline
        // passes), silently skipping any `Pending` heads-up along the way
        // — every recorded change opens with one of those before its own
        // window ever flushes, and this test only cares about flushes.
        fn recv_flushed(rx: &mpsc::Receiver<WatchSignal>) -> Option<WatchBatch> {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return None;
                }
                match rx.recv_timeout(remaining) {
                    Ok(WatchSignal::Flushed(batch)) => return Some(batch),
                    Ok(WatchSignal::Pending) => continue,
                    _ => return None,
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let staged = outside.path().join("newmod");
        std::fs::create_dir_all(&staged).unwrap();

        let (tx, rx) = mpsc::channel();
        spawn(dir.path().to_path_buf(), tx, DEBOUNCE_QUIET);

        // Drain the guaranteed startup catch-up flush (see `spawn`'s docs)
        // so it can't be mistaken for either flush this test waits on.
        assert!(
            recv_flushed(&rx).is_some(),
            "expected the guaranteed startup catch-up flush"
        );

        std::fs::rename(&staged, dir.path().join("newmod")).unwrap();
        // Let the rename's own flush land on its own first, so the write
        // below can only be satisfying a *second*, distinct flush — proof
        // the directory itself is now watched, not just that this one
        // rename event was classified and debounced correctly.
        assert!(
            recv_flushed(&rx).is_some(),
            "the rename event itself must flush"
        );

        std::fs::write(dir.path().join("newmod").join("after_rename.txt"), "x\n").unwrap();
        assert!(
            recv_flushed(&rx).is_some(),
            "a write inside a directory renamed into the tree must flush — \
             the directory must have gotten its own watch when it arrived"
        );
    }

    /// Finding #1's dedup guard: once a directory is registered this
    /// session, a later event on that exact path (an ordinary metadata-only
    /// `Modify`, say — a child being added bumps the parent's mtime) must
    /// short-circuit before `register_subtree`'s filesystem walk runs
    /// again, which is exactly the perf regression the finding's own fix
    /// note warned a naive "trigger on any `ChangeKind`" widening would
    /// otherwise reintroduce. Observed through `debounce`:
    /// `maybe_register_new_dir` only ever calls `debounce.record` when it
    /// actually attempts registration, so a second call that stays a no-op
    /// leaves `debounce` exactly as empty as it started.
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    #[test]
    fn a_directory_already_registered_this_session_is_not_rewalked() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = WatchSession::start(dir.path().to_path_buf()).unwrap();
        let sub = dir.path().join("subdir");
        std::fs::create_dir_all(&sub).unwrap();

        let start = Instant::now();
        let mut debounce = Debounce::new(DEBOUNCE_QUIET, DEBOUNCE_MAX_LATENCY);
        session.maybe_register_new_dir(&sub, ChangeKind::Created, &mut debounce, start);
        assert!(
            !debounce.is_empty(),
            "the first sighting of a new directory must register and record it"
        );
        debounce.flush();
        assert!(debounce.is_empty());

        session.maybe_register_new_dir(&sub, ChangeKind::Changed, &mut debounce, start);
        assert!(
            debounce.is_empty(),
            "an already-registered directory must short-circuit before \
             recording anything, proving the walk didn't run again"
        );
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
