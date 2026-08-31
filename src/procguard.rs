//! A process-wide, cross-thread registry of every spawned child process
//! (language servers, the ACP agent adapter) that survives even when the
//! thread owning its `Transport` never gets to run that `Transport`'s own
//! `Drop` impl.
//!
//! Both `lsp::transport::Transport` and `acp::transport::Transport` already
//! kill their child on `Drop`, and both `LspManager::shutdown_all` and
//! `AgentStore::shutdown` call that explicitly on a clean exit from
//! [`crate::ui::run`] — but neither path runs if the main thread panics
//! first. The default panic runtime, once the panic hook returns, tears
//! down the *whole process* without ever unwinding any other thread's
//! stack — and every spawned child's `Transport` lives on its own manager
//! thread's stack (`lsp::LspManager`'s worker, `acp::session`'s manager
//! thread), not the panicking one. So a panic mid-session orphans whatever
//! `npx`/`claude-agent-acp`/language-server child was running, silently,
//! with no destructor ever given the chance to fire — worse for the ACP
//! adapter than for an idle language server, since it can still be mid tool
//! call against an external API when it's orphaned.
//!
//! This registry is the last-resort net for exactly that case: each
//! `Transport::spawn` registers a [`Weak`] reference to its shared child
//! handle here, and [`kill_all`] — called only from the panic hook (see
//! `ui::install_panic_hook`) — walks the list and best-effort kills
//! whatever is still alive. A clean exit never touches this module at all;
//! the graceful shutdown paths remain the only thing that runs on that
//! path, this is purely insurance for the path that skips them.

use std::process::Child;
use std::sync::{Arc, Mutex, Weak};

/// The actual list-plus-kill logic, factored out of the process-wide
/// [`REGISTRY`] singleton so tests can exercise it against a private
/// instance instead — see the `tests` module docs on why sharing the real
/// static across the crate's whole test binary would be actively unsafe to
/// test against (`kill_all` doesn't discriminate whose child it's killing).
struct Registry {
    entries: Mutex<Vec<Weak<Mutex<Child>>>>,
}

impl Registry {
    const fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    /// Sweeps already-dead entries out of the list first: nothing else ever
    /// prunes it, and a long-running session can spawn (and normally clean
    /// up) more than one language server or agent respawn over its
    /// lifetime, so an unbounded list would otherwise just accumulate
    /// dangling `Weak`s.
    fn register(&self, child: &Arc<Mutex<Child>>) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|weak| weak.strong_count() > 0);
        entries.push(Arc::downgrade(child));
    }

    fn kill_all(&self) {
        let handles: Vec<_> = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for handle in handles {
            if let Ok(mut child) = handle.lock() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

static REGISTRY: Registry = Registry::new();

/// Registers a spawned child's shared handle. Called once per `Transport`,
/// right after `Command::spawn` succeeds — a `Weak` reference costs nothing
/// once the `Transport` (and its `Arc<Mutex<Child>>`) is later dropped
/// normally, since [`kill_all`] simply skips anything that no longer
/// upgrades.
pub fn register(child: &Arc<Mutex<Child>>) {
    REGISTRY.register(child);
}

/// Best-effort kill-and-wait on every child process still registered.
/// Intended for exactly one caller: the panic hook, as the last-resort net
/// described in the module docs above. Never called on a normal exit —
/// `LspManager::shutdown_all`/`AgentStore::shutdown` already handle that
/// path gracefully (drain, then kill only what's left), and calling this
/// too would just be a redundant, non-graceful kill of the same processes.
pub fn kill_all() {
    REGISTRY.kill_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // Deliberately against a private `Registry::new()`, never the real
    // process-wide `REGISTRY` — `cargo test` runs every unit test in the
    // crate in one process, and any test elsewhere that spawns a real
    // `acp`/`lsp` `Transport` registers into that same static via
    // production code. `kill_all` doesn't know or care whose child it's
    // killing, so exercising the real singleton here could reach out and
    // kill a live process some unrelated, concurrently-running test still
    // needs — a private instance gets the exact same logic under test with
    // no risk of touching anything outside this test.
    fn spawn_sleeper() -> Arc<Mutex<Child>> {
        let child = Command::new("sleep")
            .arg("5")
            .spawn()
            .expect("spawn sleep(1) for the test");
        Arc::new(Mutex::new(child))
    }

    #[test]
    fn kill_all_terminates_a_registered_child() {
        let registry = Registry::new();
        let child = spawn_sleeper();
        let pid = child.lock().unwrap().id();
        registry.register(&child);

        registry.kill_all();

        let exited = child.lock().unwrap().try_wait().unwrap().is_some();
        assert!(exited, "pid {pid} should have exited after kill_all");
    }

    #[test]
    fn kill_all_skips_a_child_whose_transport_already_dropped() {
        // A `Weak` for a `Transport` that already ran its own `Drop`-time
        // kill (the normal-exit path) must not make `kill_all` panic or
        // otherwise misbehave — this is the "nothing to do" half of the
        // last-resort net, exercised the same way a clean shutdown leaves
        // the registry after every real session.
        let registry = Registry::new();
        let child = spawn_sleeper();
        registry.register(&child);
        let mut guard = child.lock().unwrap();
        let _ = guard.kill();
        let _ = guard.wait();
        drop(guard);
        drop(child); // only the registry's Weak is left now

        registry.kill_all(); // must not panic
    }

    #[test]
    fn register_prunes_dead_weak_refs_so_the_registry_does_not_grow_unbounded() {
        let registry = Registry::new();
        let first = spawn_sleeper();
        registry.register(&first);
        first.lock().unwrap().kill().unwrap();
        first.lock().unwrap().wait().unwrap();
        drop(first);

        let before = registry.entries.lock().unwrap().len();
        let second = spawn_sleeper();
        registry.register(&second);
        let after = registry.entries.lock().unwrap().len();

        // `first`'s dropped Arc is swept away by this `register` call, so
        // the list doesn't just grow by one — it stays flat (the dead entry
        // pruned, the live one added) rather than accumulating.
        assert!(
            after <= before,
            "registering after a dropped child should prune, not just grow \
             (before={before}, after={after})"
        );

        registry.kill_all(); // clean up `second`
    }
}
