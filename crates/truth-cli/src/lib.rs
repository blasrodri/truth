//! Library surface for the `truth` CLI, so commands are unit/integration
//! testable without spawning the binary.

pub mod ask;
pub mod baseline;
pub mod check;
pub mod ci;
pub mod claims;
pub mod commands;
pub mod config_util;
pub mod diagnostics;
pub mod diff;
pub mod diff_facts;
pub mod docs;
pub mod doctor;
pub mod eval;
pub mod explain;
pub mod hook;
pub mod inspect;
pub mod owners;
pub mod refs;
pub mod report;
pub mod run;
pub mod service;
pub mod settings;
pub mod stats;
pub mod verify_turn;
