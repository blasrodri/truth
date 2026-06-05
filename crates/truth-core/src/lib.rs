//! `truth-core` — domain models, enums, config, verdict engine and adapter
//! traits for the `truth` claim checker.

pub mod claim;
pub mod concept;
pub mod config;
pub mod enums;
pub mod models;
pub mod query;
pub mod report;
pub mod traits;
pub mod verdict;

pub use config::Config;

use uuid::Uuid;

/// A fresh UUIDv4 string, used for all entity ids.
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

/// Current time as unix epoch seconds.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}
