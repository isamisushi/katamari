//! Shared support code for katamari's PTY-driven E2E suite: a harness that
//! spawns the real `ktmr` binary in a fake terminal, a `Key` vocabulary so
//! test bodies never contain raw escape bytes, and throwaway git fixtures
//! for it to review. See `harness::Harness`'s docs for the design and the
//! one ordering guarantee every test in the suite leans on.

pub mod fixture;
pub mod harness;
pub mod key;
pub mod mouse;
pub mod screen;

pub use harness::{Harness, KittyMode, SpawnOptions};
pub use key::Key;
pub use mouse::{MouseButton, MouseKey};
