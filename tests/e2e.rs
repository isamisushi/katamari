//! Katamari's real-binary E2E suite (M10): drives the actual compiled
//! `ktmr` through a PTY (see `support::Harness`) instead of exercising
//! `App`/`FileView` in-process, so it catches whatever only shows up once
//! crossterm, a real terminal's byte stream, and the kitty keyboard
//! protocol negotiation are all actually involved.
//!
//! One test binary (`cargo test --test e2e`) rather than one per file: the
//! `support` module is sizeable enough that compiling it once, shared by
//! every test function below, is worth more than the marginal isolation a
//! separate binary per file would buy.

mod support;

// `tests/e2e.rs` is this test binary's crate root, so a bare `mod kitty;`
// would resolve to `tests/kitty.rs` — which cargo would then also
// auto-discover as a second, separate test binary. `#[path]` keeps the
// files tucked under `tests/e2e/` (invisible to that auto-discovery, which
// only scans `tests/*.rs` directly) while still splitting them out of this
// one file.
#[path = "e2e/doctor.rs"]
mod doctor;
#[path = "e2e/file_tree.rs"]
mod file_tree;
#[path = "e2e/focus.rs"]
mod focus;
#[path = "e2e/fold.rs"]
mod fold;
#[path = "e2e/help.rs"]
mod help;
#[path = "e2e/kitty.rs"]
mod kitty;
#[path = "e2e/log.rs"]
mod log;
#[path = "e2e/lsp_inspector.rs"]
mod lsp_inspector;
#[path = "e2e/lsp_readiness.rs"]
mod lsp_readiness;
#[path = "e2e/navigation.rs"]
mod navigation;
#[path = "e2e/rendering.rs"]
mod rendering;
#[path = "e2e/scope_menu.rs"]
mod scope_menu;
#[path = "e2e/search.rs"]
mod search;
#[path = "e2e/show_keys.rs"]
mod show_keys;
#[path = "e2e/skill_install.rs"]
mod skill_install;
#[path = "e2e/update_check.rs"]
mod update_check;
#[path = "e2e/watch.rs"]
mod watch;
#[path = "e2e/wrap.rs"]
mod wrap;
