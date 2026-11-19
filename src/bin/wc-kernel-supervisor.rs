//! Internal process-tree supervisor for the WordCraft Kernel runner.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable
)]
#![forbid(unsafe_code)]

#[path = "../supervisor.rs"]
mod supervisor;

use std::io::Write;
use std::process::ExitCode;

fn main() -> ExitCode {
    match supervisor::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "wc-kernel-supervisor: {error}");
            ExitCode::from(125)
        }
    }
}
