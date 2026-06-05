//! Library surface for the `truth` CLI, so commands are unit/integration
//! testable without spawning the binary.

pub mod baseline;
pub mod check;
pub mod ci;
pub mod claims;
pub mod commands;
pub mod config_util;
pub mod diagnostics;
pub mod diff;
pub mod doctor;
pub mod eval;
pub mod explain;
pub mod inspect;
pub mod owners;
pub mod refs;
pub mod report;
pub mod service;
