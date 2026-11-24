//! Cataly: the hub, its command line and the job engine.
//!
//! The library target exists so that integration tests can drive the same code as the binary; every
//! module is private to the crate.

mod cli;
mod config;
mod hub;
mod jobs;
mod log;
mod proto;
mod store;
mod sys;

/// Runs the `cataly` command line with the process arguments and returns its exit status.
pub fn run() -> std::process::ExitCode {
    cli::main()
}
