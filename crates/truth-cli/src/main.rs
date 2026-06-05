//! `truth` CLI — deterministic engineering claim/evidence checker.

use truth_cli::{commands, eval, explain};

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "truth",
    about = "A fact-checker for engineering teams: claims checked against code, config, and logs.",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize truth config and database.
    Init,
    /// Index repo docs/code/config.
    Index {
        /// Path to index (defaults to ".").
        #[arg(default_value = ".")]
        path: String,
    },
    /// Check a natural-language engineering claim.
    Check {
        /// The claim to check, e.g. "nobody uses /v1/checkout anymore".
        claim: String,
        /// Use a local log file for the log source (offline, when Loki is off).
        #[arg(long)]
        local_log: Option<String>,
        /// Emit machine-readable JSON only.
        #[arg(long)]
        json: bool,
    },
    /// Check observed usage of a route/event/pattern.
    Usage {
        subject: String,
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        local_log: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Check error occurrences.
    Errors {
        pattern: String,
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        local_log: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Find latest occurrence of a pattern.
    Latest {
        pattern: String,
        #[arg(long)]
        window: Option<String>,
        #[arg(long)]
        env: Option<String>,
        #[arg(long)]
        service: Option<String>,
        #[arg(long)]
        local_log: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Search indexed config/code definitions.
    Config {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Explain a previous check from the audit trail.
    Explain {
        check_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Run an evaluation fixture (YAML).
    Eval {
        fixture: String,
        #[arg(long)]
        json: bool,
    },
    /// Database commands.
    Db {
        #[command(subcommand)]
        cmd: DbCommand,
    },
    /// Placeholder for future Slack/server mode.
    Serve,
}

#[derive(Subcommand)]
enum DbCommand {
    /// Run SQLite migrations.
    Migrate,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init => commands::init(),
        Command::Index { path } => commands::index(&path),
        Command::Check { claim, local_log, json } => {
            commands::check(&claim, local_log.as_deref(), json)
        }
        Command::Usage {
            subject,
            window,
            env,
            service,
            local_log,
            json,
        } => commands::usage(&subject, window.as_deref(), env.as_deref(), service.as_deref(), local_log.as_deref(), json),
        Command::Errors {
            pattern,
            window,
            env,
            service,
            local_log,
            json,
        } => commands::errors(&pattern, window.as_deref(), env.as_deref(), service.as_deref(), local_log.as_deref(), json),
        Command::Latest {
            pattern,
            window,
            env,
            service,
            local_log,
            json,
        } => commands::latest(&pattern, window.as_deref(), env.as_deref(), service.as_deref(), local_log.as_deref(), json),
        Command::Config { key, json } => commands::config(&key, json),
        Command::Explain { check_id, json } => explain::explain(&check_id, json),
        Command::Eval { fixture, json } => eval::eval(&fixture, json),
        Command::Db { cmd } => match cmd {
            DbCommand::Migrate => commands::db_migrate(),
        },
        Command::Serve => {
            println!("`truth serve` (Slack/HTTP) is not part of this build.");
            Ok(())
        }
    }
}
