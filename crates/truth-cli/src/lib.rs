//! Library surface for the `truth` CLI, so commands are unit/integration
//! testable without spawning the binary.

pub mod check;
pub mod commands;
pub mod eval;
pub mod explain;
pub mod service;
