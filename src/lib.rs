//! Cataly: the hub, its command line, the job engine with the owner jobs, the OMP probe and Discord
//! posting.
//!
//! The library target exists so that integration tests can drive the same code as the binary; every
//! module is private to the crate.

mod cli;
mod config;
mod hub;
mod jobs;
mod log;
mod omp;
mod proto;
mod relay;
mod store;
mod sys;

/// Runs the `cataly` command line with the process arguments and returns its exit status.
pub fn run() -> std::process::ExitCode {
    cli::main()
}
