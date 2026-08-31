//! ACP (Agent Client Protocol, agentclientprotocol.com) client — katamari
//! as the *client* side of the editor↔agent protocol, spawning and owning
//! a persistent agent process the way it already owns language servers.
//!
//! Why this exists: review comments were pull-only — an agent had to run
//! `ktmr comments list` to learn about them. Pushing into a user's
//! already-open interactive Claude Code session is not supported by any
//! stable surface (the per-session messaging socket's protocol is
//! unpublished and trust is process-ancestry-scoped), so katamari inverts
//! the relationship instead: it hosts its own resident agent session over
//! ACP and prompts it with review comments directly. The agent edits the
//! working tree with its own tools, katamari's live refresh shows those
//! edits as they land, and `ktmr comments resolve` closes the loop — the
//! entire review cycle happens inside one `ktmr diff`.
//!
//! Spike status: the protocol layer and a headless `ktmr agent-check`
//! prove the loop; TUI integration (attach a session to the diff view,
//! push on comment save, stream progress in a pane) builds on this.

pub mod adapter;
pub mod check;
pub mod client;
pub mod session;
pub mod transport;
