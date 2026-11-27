//! `#[path]` shims for the graph-service test target. `dev` internals are
//! private to the `ns dev` command, so the modules the engine needs are
//! compiled into the test crate directly; `use super::x` inside them
//! resolves to these siblings, exactly as inside `src/dev/mod.rs`.
//!
//! Not a test target itself (cargo only picks up `tests/*/main.rs`).

#[path = "../../src/dev/ansi_strip.rs"]
pub mod ansi_strip;
#[path = "../../src/dev/go_list.rs"]
pub mod go_list;
#[path = "../../src/dev/process_supervisor.rs"]
pub mod process_supervisor;
#[path = "../../src/dev/log_bus.rs"]
pub mod log_bus;
#[path = "../../src/dev/status_line.rs"]
pub mod status_line;
#[path = "../../src/dev/go_dev.rs"]
pub mod go_dev;
#[path = "../../src/dev/file_watcher.rs"]
pub mod file_watcher;
