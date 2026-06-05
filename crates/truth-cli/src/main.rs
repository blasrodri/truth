//! `truth` CLI — deterministic engineering claim/evidence checker.

use truth_cli::{baseline, commands, doctor, eval, explain, inspect};

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
    /// Validate local setup and explain readiness.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Show what was indexed (summary, or routes/constants/env/ports/deps/evidence).
    Inspect {
        /// Optional category to list.
        category: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run auto-generated baseline checks from indexed evidence + logs.
    Baseline {
        #[arg(long)]
        local_log: Option<String>,
        #[arg(long)]
        json: bool,
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
        /// Record actual outputs to a YAML file instead of asserting.
        #[arg(long)]
        record: Option<String>,
        /// Overwrite the record file if it already exists.
        #[arg(long)]
        force: bool,
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
        Command::Doctor { json } => doctor::doctor(json),
        Command::Inspect { category, json } => inspect::inspect(category.as_deref(), json),
        Command::Baseline { local_log, json } => baseline::baseline(local_log.as_deref(), json),
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
        Command::Eval { fixture, json, record, force } => {
            eval::eval(&fixture, json, record.as_deref(), force)
        }
        Command::Db { cmd } => match cmd {
            DbCommand::Migrate => commands::db_migrate(),
        },
        Command::Serve => commands::serve(),
    }
}
