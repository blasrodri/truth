//! `truth` CLI — deterministic engineering claim/evidence checker.

use truth_cli::{
    ask, baseline, ci, claims, commands, diff, docs, doctor, eval, explain, inspect, owners, refs,
    report, settings, verify_turn,
};

use clap::{Parser, Subcommand};
use std::process::ExitCode;

// Heap profiler (feature `dhat-heap`). When active, writes dhat-heap.json on
// exit with exact allocation counts/bytes per call site.
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

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
    /// One-command onboarding for this repo: init + Claude Code hooks + MCP
    /// registration. Run it once per repo; everything after is automatic.
    Setup,
    /// Index repo docs/code/config.
    Index {
        /// Path to index (defaults to ".").
        #[arg(default_value = ".")]
        path: String,
        /// Print indexing throughput and recall statistics.
        #[arg(long)]
        stats: bool,
        /// Force a full re-index instead of the incremental (skip-unchanged) path.
        #[arg(long)]
        full: bool,
        /// Extraction backend: regex (default) | ast | mixed. Overrides config.
        #[arg(long)]
        extractor: Option<String>,
    },
    /// Validate local setup and explain readiness.
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Show what was indexed (summary, or routes/constants/env/ports/deps/evidence).
    Inspect {
        /// Optional category to list (routes/constants/env/ports/deps/evidence/extraction).
        category: Option<String>,
        /// Filter by extraction method: ast | regex | all (default all).
        #[arg(long)]
        source: Option<String>,
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
    /// Audit your own Claude Code session history — show the over-claims truth
    /// would have caught, each checked against the code AS IT WAS at the time.
    AuditSession {
        /// How many recent sessions to audit (newest first).
        #[arg(long, default_value_t = 5)]
        last: usize,
        /// Restrict to sessions whose repo is this path.
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Check GitHub for a newer truth release (notify only — never downloads).
    UpgradeCheck,
    /// Run a command and record a receipt (exit code, time, output digest) so
    /// success claims like "tests pass" become verifiable. Exits with the
    /// command's own exit code.
    Run {
        /// The command and its args: `truth run -- cargo test`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record a receipt for a command that already ran elsewhere (agent
    /// hooks, CI). Never executes anything.
    RecordRun {
        /// The command line that ran.
        #[arg(long)]
        command: String,
        #[arg(long)]
        exit_code: i64,
        /// test|build|lint|typecheck|other (inferred from the command if omitted).
        #[arg(long)]
        kind: Option<String>,
        /// Repo root whose .truth store records it (defaults to the discovered root).
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Fact-check an AI agent's message about its own work (multi-claim).
    VerifyTurn {
        /// What the agent said, e.g. "I added /v1/refund and bumped the timeout to 30s".
        message: String,
        /// Repo root to verify against (opens <repo>/.truth). Defaults to CWD config.
        #[arg(long)]
        repo: Option<String>,
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
    /// Show who has worked on the code behind a subject (route/key/file).
    Owners {
        subject: String,
        #[arg(long)]
        json: bool,
    },
    /// One fused answer about a file: owners, activity, who to ask.
    Ask {
        file: String,
        #[arg(long)]
        json: bool,
    },
    /// Find code references to a symbol/route/dependency (is it actually used?).
    Uses {
        symbol: String,
        #[arg(long)]
        json: bool,
    },
    /// Check if a subject is present in the docs (and consistent with code).
    Docs {
        subject: String,
        #[arg(long)]
        json: bool,
    },
    /// Explain a previous check from the audit trail.
    Explain {
        check_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Run an evaluation fixture (YAML; accepts `cases:` or `claims:`).
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
        /// Precision-gate mode: report false-contradictions (must be 0) and
        /// recall (advisory), and exit non-zero ONLY on a false contradiction.
        #[arg(long)]
        precision: bool,
    },
    /// Extract candidate engineering claims from docs/text into a claim file.
    Claims {
        /// Files or directories to scan.
        paths: Vec<String>,
        /// Write the claim file here (otherwise prints to stdout).
        #[arg(long)]
        out: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Run checks from a claim file and produce a report.
    Report {
        claim_file: String,
        #[arg(long)]
        local_log: Option<String>,
        /// text | markdown | json
        #[arg(long, default_value = "text")]
        format: String,
        #[arg(long)]
        out: Option<String>,
    },
    /// Run checks from a claim file and exit per a CI fail policy.
    Ci {
        claim_file: String,
        #[arg(long)]
        local_log: Option<String>,
        /// Comma-separated statuses that count as failing, e.g. contradicted,inconclusive
        #[arg(long)]
        fail_on: Option<String>,
        /// Minimum severity that participates in failure: info|warning|error
        #[arg(long)]
        fail_severity: Option<String>,
        #[arg(long)]
        format: Option<String>,
        #[arg(long)]
        out: Option<String>,
    },
    /// Compare two reports or recorded eval outputs.
    Diff {
        old: String,
        new: String,
        #[arg(long)]
        json: bool,
    },
    /// Agent-harness hooks: make verification a gate the agent can't skip.
    Hook {
        #[command(subcommand)]
        cmd: HookCommand,
    },
    /// Database commands.
    Db {
        #[command(subcommand)]
        cmd: DbCommand,
    },
    /// The lie ledger: claims checked, contradictions caught, runs recorded.
    Stats {
        /// Time window, e.g. 7d, 24h, 30d.
        #[arg(long)]
        window: Option<String>,
        /// Aggregate across all repos registered by `truth init`.
        #[arg(long)]
        all: bool,
        /// Re-run each past contradiction through the CURRENT engine and flag
        /// the ones that no longer contradict — phantom false-positives an
        /// engine fix has already resolved. Self-auditing the lie ledger.
        #[arg(long)]
        review: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read or change truth.toml settings (extractor, include/exclude, llm, ...).
    Settings {
        #[command(subcommand)]
        cmd: SettingsCommand,
    },
    /// Placeholder for future Slack/server mode.
    Serve,
}

#[derive(Subcommand)]
enum DbCommand {
    /// Run SQLite migrations.
    Migrate,
}

#[derive(Subcommand)]
enum HookCommand {
    /// Register truth's Stop + PostToolUse hooks in Claude Code settings
    /// (.claude/settings.json; --user for ~/.claude/settings.json).
    Install {
        /// Install in ~/.claude/settings.json instead of the project settings.
        #[arg(long)]
        user: bool,
        /// Install only the receipt recorders (PostToolUse/PostToolUseFailure),
        /// skipping the blocking Stop gate. Recommended with --user: receipts
        /// are pure evidence-gathering, while a blocking gate should be a
        /// per-repo decision.
        #[arg(long)]
        receipts: bool,
    },
    /// The hook entry point Claude Code invokes (reads the event from stdin).
    Claude,
    /// Toggle zero-setup: whether hooks fact-check git repos that haven't run
    /// `truth init` (default on). Modes: on | off | status.
    Auto {
        #[arg(default_value = "status")]
        mode: String,
    },
}

#[derive(Subcommand)]
enum SettingsCommand {
    /// List every configurable setting, its current value, and help.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print one setting's value.
    Get {
        key: String,
        #[arg(long)]
        json: bool,
    },
    /// Set one setting (validated; preserves the rest of truth.toml).
    Set {
        key: String,
        value: String,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> ExitCode {
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    let cli = Cli::parse();

    // `run` propagates the child command's exit code.
    if let Command::Run { cmd, json } = &cli.command {
        return match truth_cli::run::run(cmd, *json) {
            Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
            Err(e) => {
                eprintln!("Error: {e:#}");
                ExitCode::FAILURE
            }
        };
    }

    // `ci` owns its exit code policy (0 pass / 1 fail / 2 operational error).
    if let Command::Ci {
        claim_file,
        local_log,
        fail_on,
        fail_severity,
        format,
        out,
    } = &cli.command
    {
        let code = ci::ci(
            claim_file,
            local_log.as_deref(),
            fail_on.as_deref(),
            fail_severity.as_deref(),
            format.as_deref(),
            out.as_deref(),
        );
        return ExitCode::from(code as u8);
    }

    match run(cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("Error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> anyhow::Result<()> {
    match command {
        Command::Init => commands::init(),
        Command::Setup => commands::setup(),
        Command::Index {
            path,
            stats,
            full,
            extractor,
        } => commands::index(&path, stats, full, extractor.as_deref()),
        Command::Doctor { json } => doctor::doctor(json),
        Command::Inspect {
            category,
            source,
            json,
        } => inspect::inspect(category.as_deref(), source.as_deref(), json),
        Command::Baseline { local_log, json } => baseline::baseline(local_log.as_deref(), json),
        Command::Check {
            claim,
            local_log,
            json,
        } => commands::check(&claim, local_log.as_deref(), json),
        Command::AuditSession { last, repo, json } => {
            truth_cli::audit_session::audit(&truth_cli::audit_session::AuditOpts {
                last,
                repo,
                json,
            })
        }
        Command::UpgradeCheck => commands::upgrade_check(false),
        Command::Run { .. } => unreachable!("run handled before run()"),
        Command::RecordRun {
            command,
            exit_code,
            kind,
            repo,
            json,
        } => truth_cli::run::record(&command, exit_code, kind.as_deref(), repo.as_deref(), json),
        Command::VerifyTurn {
            message,
            repo,
            local_log,
            json,
        } => verify_turn::verify_turn(&message, repo.as_deref(), local_log.as_deref(), json),
        Command::Usage {
            subject,
            window,
            env,
            service,
            local_log,
            json,
        } => commands::usage(
            &subject,
            window.as_deref(),
            env.as_deref(),
            service.as_deref(),
            local_log.as_deref(),
            json,
        ),
        Command::Errors {
            pattern,
            window,
            env,
            service,
            local_log,
            json,
        } => commands::errors(
            &pattern,
            window.as_deref(),
            env.as_deref(),
            service.as_deref(),
            local_log.as_deref(),
            json,
        ),
        Command::Latest {
            pattern,
            window,
            env,
            service,
            local_log,
            json,
        } => commands::latest(
            &pattern,
            window.as_deref(),
            env.as_deref(),
            service.as_deref(),
            local_log.as_deref(),
            json,
        ),
        Command::Config { key, json } => commands::config(&key, json),
        Command::Owners { subject, json } => owners::owners(&subject, json),
        Command::Ask { file, json } => ask::ask(&file, json),
        Command::Uses { symbol, json } => refs::uses(&symbol, json),
        Command::Docs { subject, json } => docs::docs(&subject, json),
        Command::Explain { check_id, json } => explain::explain(&check_id, json),
        Command::Eval {
            fixture,
            json,
            record,
            force,
            precision,
        } => eval::eval(&fixture, json, record.as_deref(), force, precision),
        Command::Claims { paths, out, json } => claims::claims(&paths, out.as_deref(), json),
        Command::Report {
            claim_file,
            local_log,
            format,
            out,
        } => report::report(&claim_file, local_log.as_deref(), &format, out.as_deref()),
        Command::Diff { old, new, json } => diff::diff(&old, &new, json),
        Command::Stats {
            window,
            all,
            review,
            json,
        } => {
            if review {
                truth_cli::stats::review(window.as_deref(), json)
            } else {
                truth_cli::stats::stats(window.as_deref(), all, json)
            }
        }
        Command::Ci { .. } => unreachable!("ci handled before run()"),
        Command::Settings { cmd } => match cmd {
            SettingsCommand::List { json } => settings::list(json),
            SettingsCommand::Get { key, json } => settings::get(&key, json),
            SettingsCommand::Set { key, value, json } => settings::set(&key, &value, json),
        },
        Command::Hook { cmd } => match cmd {
            HookCommand::Install { user, receipts } => truth_cli::hook::install(user, receipts),
            HookCommand::Claude => truth_cli::hook::claude(),
            HookCommand::Auto { mode } => truth_cli::hook::auto(&mode),
        },
        Command::Db { cmd } => match cmd {
            DbCommand::Migrate => commands::db_migrate(),
        },
        Command::Serve => commands::serve(),
    }
}
