//! `truth-logs` — queryable log sources (Loki + local file) and redaction.

pub mod localfile;
pub mod loki;
pub mod redact;
pub mod window;

pub use localfile::LocalFileSource;
pub use loki::LokiSource;
