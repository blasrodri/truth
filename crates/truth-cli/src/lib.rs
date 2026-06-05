//! Library surface for the `truth` CLI, so commands are unit/integration
//! testable without spawning the binary.

pub mod baseline;
pub mod check;
pub mod commands;
pub mod config_util;
pub mod diagnostics;
pub mod doctor;
pub mod eval;
pub mod explain;
pub mod inspect;
pub mod service;
