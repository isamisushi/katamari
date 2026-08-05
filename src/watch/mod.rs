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
pub fn spawn(repo_root: PathBuf, tx: Sender<WatchSignal>) -> notify::Result<()> {
    let session = WatchSession::start(repo_root)?;
    std::thread::spawn(move || session.run(tx));
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
    fn run(self, tx: Sender<WatchSignal>) {
        let start = Instant::now();
        let mut debounce = Debounce::new(DEBOUNCE_QUIET, DEBOUNCE_MAX_LATENCY);

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
}
