//! Shared config/JSON helpers used across CLI command modules.

use anyhow::Result;
use std::path::Path;
use truth_core::config::Config;

/// Load `truth.toml` from the current directory, falling back to built-in
/// defaults when it is absent.
pub fn load_config() -> Result<Config> {
    if Path::new("truth.toml").exists() {
        Config::load("truth.toml")
    } else {
        Ok(Config::from_toml_str("")?)
    }
}

/// Pretty-print a JSON value to stdout.
pub fn print_json(v: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(v).expect("json serializes")
    );
}
