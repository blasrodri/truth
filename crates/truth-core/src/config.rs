//! `truth.toml` configuration (spec §10). Secrets are referenced indirectly via
//! `*_env` keys that name an environment variable.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub database: DatabaseConfig,
    #[serde(default)]
    pub repo: RepoConfig,
    #[serde(default)]
    pub slack: SlackConfig,
    #[serde(default)]
    pub llm: LlmConfig,
    #[serde(default)]
    pub loki: LokiConfig,
    #[serde(default)]
    pub security: SecurityConfig,
    #[serde(default)]
    pub verdict: VerdictConfig,
    #[serde(default)]
    pub indexer: IndexerConfig,
}

/// Which extraction backend the indexer uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExtractorMode {
    /// Regex-only (current behavior, the conservative default).
    #[default]
    Regex,
    /// AST for supported languages (Rust routes); regex for everything else.
    Ast,
    /// AST where available, regex where not. AST routes win over regex routes.
    Mixed,
}

impl ExtractorMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ExtractorMode::Regex => "regex",
            ExtractorMode::Ast => "ast",
            ExtractorMode::Mixed => "mixed",
        }
    }

    /// Parse from a CLI flag / config string.
    pub fn parse(s: &str) -> Option<ExtractorMode> {
        match s.to_ascii_lowercase().as_str() {
            "regex" => Some(ExtractorMode::Regex),
            "ast" => Some(ExtractorMode::Ast),
            "mixed" => Some(ExtractorMode::Mixed),
            _ => None,
        }
    }

    /// Whether AST extraction runs for supported languages under this mode.
    pub fn uses_ast(&self) -> bool {
        matches!(self, ExtractorMode::Ast | ExtractorMode::Mixed)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IndexerConfig {
    #[serde(default)]
    pub extractor: ExtractorMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub path: String,
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: ".truth/truth.sqlite".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub root: String,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}
impl Default for RepoConfig {
    fn default() -> Self {
        Self {
            root: ".".into(),
            include: vec![
                "src".into(),
                "docs".into(),
                "README.md".into(),
                "Cargo.toml".into(),
                "package.json".into(),
                "docker-compose.yml".into(),
            ],
            exclude: vec!["target".into(), "node_modules".into(), ".git".into()],
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConfig {
    pub signing_secret_env: Option<String>,
    pub bot_token_env: Option<String>,
    pub app_token_env: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub provider: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub timeout_ms: u64,
    /// When false, always use the deterministic extractor.
    #[serde(default = "default_true")]
    pub enabled: bool,
}
impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: "openai_compatible".into(),
            base_url: "http://localhost:11434/v1".into(),
            model: "qwen3:1.7b".into(),
            api_key_env: Some("LLM_API_KEY".into()),
            // Generous by default: a cold local model's first token can take
            // many seconds. The extractor falls back to regex on timeout.
            timeout_ms: 30_000,
            enabled: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LokiConfig {
    pub enabled: bool,
    pub base_url: String,
    pub default_env: String,
    pub default_window: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
}
impl Default for LokiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            base_url: "http://localhost:3100".into(),
            default_env: "prod".into(),
            default_window: "7d".into(),
            labels: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub max_log_window_days: u32,
    pub include_log_samples: bool,
    pub max_log_samples: usize,
    pub redact_pii: bool,
    pub allowed_environments: Vec<String>,
}
impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_log_window_days: 30,
            include_log_samples: true,
            max_log_samples: 3,
            redact_pii: true,
            allowed_environments: vec!["prod".into(), "staging".into()],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictConfig {
    pub default_confidence_threshold: f32,
}
impl Default for VerdictConfig {
    fn default() -> Self {
        Self {
            default_confidence_threshold: 0.75,
        }
    }
}

fn default_true() -> bool {
    true
}

impl Config {
    pub fn from_toml_str(s: &str) -> anyhow::Result<Self> {
        Ok(toml::from_str(s)?)
    }

    pub fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let s = std::fs::read_to_string(path.as_ref())
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.as_ref().display()))?;
        Self::from_toml_str(&s)
    }

    /// Resolve the value of an `*_env`-named environment variable, if set.
    pub fn resolve_env(name: &Option<String>) -> Option<String> {
        name.as_ref().and_then(|n| std::env::var(n).ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let c = Config::from_toml_str("").unwrap();
        assert_eq!(c.server.port, 8080);
        assert_eq!(c.database.path, ".truth/truth.sqlite");
        assert_eq!(c.security.max_log_window_days, 30);
    }

    #[test]
    fn example_config_parses() {
        let toml = r#"
[server]
host = "0.0.0.0"
port = 8080

[loki]
enabled = true
base_url = "http://localhost:3100"
default_env = "prod"
default_window = "7d"

[loki.labels]
env = "env"
service = "service"
"#;
        let c = Config::from_toml_str(toml).unwrap();
        assert!(c.loki.enabled);
        assert_eq!(c.loki.labels.get("env").map(String::as_str), Some("env"));
    }
}
