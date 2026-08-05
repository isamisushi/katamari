//! Trailing-edge coalescing for a burst of filesystem events into one batch
//! per "the user stopped touching files for a moment." Kept free of any real
//! clock or thread: [`Debounce`] is fed `(elapsed-since-start, path, kind)`
//! tuples and asked `should_flush` at a given elapsed time, so tests can
//! drive it with synthetic [`Duration`]s instead of sleeping — the watcher
//! thread (see [`crate::watch`]) is the only caller that ever hands it a
//! real [`std::time::Instant::elapsed`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

/// How a watched path changed, coarse enough to map directly onto LSP's
/// `FileChangeType` (see [`crate::ui`]'s watch-refresh handling) without
/// needing notify's much finer-grained [`notify::EventKind`] beyond this
/// classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    Created,
    Changed,
    Deleted,
}

/// One path's net change within a debounce window — if a path was touched
/// more than once before the window flushed, only its most recent
/// [`ChangeKind`] survives; a reviewer doesn't care that a file was written
/// three times in the last 200ms, only that it's different now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedPath {
    pub path: PathBuf,
    pub kind: ChangeKind,
}

/// Trailing-edge debounce with a max-latency backstop: a window stays open
/// as long as new events keep arriving within `quiet_period` of each other,
/// and flushes `quiet_period` after the last one — but never waits longer
/// than `max_latency` from the *first* event in the window, so a continuous
/// stream of writes (a build tool re-touching files in a loop, say) still
/// produces periodic refreshes instead of starving the window closed
/// forever.
pub struct Debounce {
    quiet_period: Duration,
    max_latency: Duration,
    /// Insertion order of paths currently pending, so a flush's batch lists
    /// changes in the order they were first seen rather than in
    /// [`HashMap`]'s unspecified iteration order — nothing downstream
    /// depends on this ordering today, but it makes `ktmr watch-check`'s
    /// output deterministic to read.
    order: Vec<PathBuf>,
    kinds: HashMap<PathBuf, ChangeKind>,
    window_start: Option<Duration>,
    last_event: Option<Duration>,
}

impl Debounce {
    pub fn new(quiet_period: Duration, max_latency: Duration) -> Self {
        Self {
            quiet_period,
            max_latency,
            order: Vec::new(),
            kinds: HashMap::new(),
            window_start: None,
            last_event: None,
        }
    }

    /// Records one change at elapsed time `at`. Opens a new window if none
    /// is currently pending; otherwise extends the current one.
    pub fn record(&mut self, at: Duration, path: PathBuf, kind: ChangeKind) {
        self.window_start.get_or_insert(at);
        self.last_event = Some(at);
        if !self.kinds.contains_key(&path) {
            self.order.push(path.clone());
        }
        self.kinds.insert(path, kind);
    }

    /// Whether any change is currently pending flush.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Whether, as of elapsed time `now`, the pending window should flush:
    /// either `quiet_period` has passed since the last recorded event, or
    /// `max_latency` has passed since the window opened. `false` with
    /// nothing pending.
    pub fn should_flush(&self, now: Duration) -> bool {
        let (Some(start), Some(last)) = (self.window_start, self.last_event) else {
            return false;
        };
        now.saturating_sub(last) >= self.quiet_period
            || now.saturating_sub(start) >= self.max_latency
    }

    /// Drains every pending change into a batch (in first-seen order) and
    /// resets the window, ready to accumulate the next one.
    pub fn flush(&mut self) -> Vec<ChangedPath> {
        let order = std::mem::take(&mut self.order);
        let mut kinds = std::mem::take(&mut self.kinds);
        self.window_start = None;
        self.last_event = None;
        order
            .into_iter()
            .map(|path| {
                let kind = kinds.remove(&path).expect("every ordered path has a kind");
                ChangedPath { path, kind }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QUIET: Duration = Duration::from_millis(200);
    const MAX_LATENCY: Duration = Duration::from_secs(1);

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn path(name: &str) -> PathBuf {
        PathBuf::from(name)
    }

    #[test]
    fn empty_debounce_never_flushes() {
        let debounce = Debounce::new(QUIET, MAX_LATENCY);
        assert!(!debounce.should_flush(ms(10_000)));
    }

    #[test]
    fn does_not_flush_before_the_quiet_period_elapses() {
        let mut debounce = Debounce::new(QUIET, MAX_LATENCY);
        debounce.record(ms(0), path("a.rs"), ChangeKind::Changed);
        assert!(!debounce.should_flush(ms(199)));
    }

    #[test]
    fn flushes_once_the_quiet_period_elapses_with_no_further_events() {
        let mut debounce = Debounce::new(QUIET, MAX_LATENCY);
        debounce.record(ms(0), path("a.rs"), ChangeKind::Changed);
        assert!(debounce.should_flush(ms(200)));
    }

    #[test]
    fn a_new_event_pushes_the_quiet_deadline_back() {
        let mut debounce = Debounce::new(QUIET, MAX_LATENCY);
        debounce.record(ms(0), path("a.rs"), ChangeKind::Changed);
        // Just before the first event's quiet deadline, a second event
        // arrives — the window must not flush at the *original* deadline.
        debounce.record(ms(150), path("b.rs"), ChangeKind::Changed);
        assert!(
            !debounce.should_flush(ms(300)),
            "should still be quiet-waiting on the second event"
        );
        assert!(
            debounce.should_flush(ms(350)),
            "200ms after the second event, it should flush"
        );
    }

    #[test]
    fn max_latency_forces_a_flush_under_continuous_writes() {
        let mut debounce = Debounce::new(QUIET, MAX_LATENCY);
        debounce.record(ms(0), path("a.rs"), ChangeKind::Changed);
        // A steady drip of events, each well within the quiet period of the
        // last, would otherwise keep the window open forever. Stop just
        // short of `MAX_LATENCY` so every intermediate check below is
        // purely about the quiet period not having elapsed yet.
        let mut t = 0u64;
        while t < 850 {
            t += 150;
            debounce.record(ms(t), path("a.rs"), ChangeKind::Changed);
            assert!(
                !debounce.should_flush(ms(t)),
                "quiet period alone shouldn't have elapsed yet at t={t}"
            );
        }
        // 1000ms after the window opened, max_latency forces a flush even
        // though the last event (at t=900) was well within the quiet
        // period.
        assert!(debounce.should_flush(ms(1_000)));
    }

    #[test]
    fn flush_drains_in_first_seen_order_with_the_latest_kind_per_path() {
        let mut debounce = Debounce::new(QUIET, MAX_LATENCY);
        debounce.record(ms(0), path("b.rs"), ChangeKind::Created);
        debounce.record(ms(10), path("a.rs"), ChangeKind::Changed);
        debounce.record(ms(20), path("b.rs"), ChangeKind::Deleted); // overwrites b.rs's kind
        let batch = debounce.flush();
        assert_eq!(
            batch,
            vec![
                ChangedPath {
                    path: path("b.rs"),
                    kind: ChangeKind::Deleted
                },
                ChangedPath {
                    path: path("a.rs"),
                    kind: ChangeKind::Changed
                },
            ]
        );
    }

    #[test]
    fn flush_resets_the_window_for_the_next_batch() {
        let mut debounce = Debounce::new(QUIET, MAX_LATENCY);
        debounce.record(ms(0), path("a.rs"), ChangeKind::Changed);
        debounce.flush();
        assert!(debounce.is_empty());
        assert!(
            !debounce.should_flush(ms(10_000)),
            "no window is open until the next record()"
        );

        debounce.record(ms(10_000), path("c.rs"), ChangeKind::Created);
        assert!(!debounce.should_flush(ms(10_050)));
        assert!(debounce.should_flush(ms(10_200)));
    }
}
