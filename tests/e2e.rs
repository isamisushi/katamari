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
#[path = "e2e/kitty.rs"]
mod kitty;
#[path = "e2e/log.rs"]
mod log;
#[path = "e2e/navigation.rs"]
mod navigation;
#[path = "e2e/rendering.rs"]
mod rendering;
#[path = "e2e/scope_menu.rs"]
mod scope_menu;
