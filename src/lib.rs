//! dispatch runner: event intake, triage, card and discussion sessions over the
//! `claude` CLI, with a machine-wide store shared by every runner.

pub mod cli;
pub mod config;
pub mod coordinator;
pub mod durations;
pub mod intake;
pub mod outcome;
pub mod paths;
pub mod prompts;
pub mod report;
pub mod results;
pub mod runner;
pub mod session;
pub mod state;
pub mod store;
pub mod worktree;

#[cfg(test)]
mod testing;
