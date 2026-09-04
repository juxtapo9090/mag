use crate::config::load_node_registry;
use crate::engine::{BatchEngine, EngineConfig};
use crate::mcp::MagServer;
use crate::render::render_toon_response;
use crate::toon::ToonRequest;
use crate::{
    AppResult, DEFAULT_CAPTURE_LIMIT_BYTES, DEFAULT_TIMEOUT_MS, DEFAULT_WORKERS, SERVER_NAME,
    SERVER_VERSION,
};
use anyhow::anyhow;
use clap::{Parser, Subcommand};
use rmcp::schemars;
use rmcp::{ServiceExt, transport::stdio};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct BatchRequest {
    pub commands: Vec<CommandSpec>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub default_cwd: Option<String>,
    #[serde(default)]
    pub default_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_parallel: Option<usize>,
    #[serde(default)]
    pub continue_on_error: Option<bool>,
    #[serde(default)]
    pub capture_limit_bytes: Option<usize>,
    #[serde(
        default,
        alias = "g0",
        alias = "g_0",
        alias = "g-0",
        alias = "ground_zero"
    )]
    pub unfiltered: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct CommandSpec {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub argv: Option<Vec<String>>,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub warm: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct BatchResponse {
    pub command_count: usize,
    pub elapsed_ms: u128,
    pub stopped_early: bool,
    pub results: Vec<CommandResult>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CommandResult {
    pub index: usize,
    pub id: Option<String>,
    pub mode: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    #[serde(default)]
    pub diet_applied: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec_user: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct ExecOutcome {
    pub mode: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
    pub error: Option<String>,
}

#[derive(Debug)]
pub struct LimitedOutput {
    pub value: Option<String>,
    pub truncated: bool,
}

#[derive(Parser, Debug)]
#[command(
    name = SERVER_NAME,
    version = SERVER_VERSION,
    about = "mag — resident warm-pool shell runner. Manual: mag-how-to.md"
)]
pub struct Cli {
    #[arg(
        long,
        default_value_t = false,
        help = "Run HTTP serve mode from YAML config"
    )]
    pub serve: bool,
    #[arg(long, help = "Serve-mode YAML config path", requires = "serve")]
    pub config: Option<PathBuf>,
    #[arg(long, help = "Optional node registry YAML for HTTP remote forwarding")]
    pub nodes: Option<PathBuf>,
    #[arg(long, default_value_t = DEFAULT_WORKERS)]
    pub workers: usize,
    #[arg(long, default_value_t = DEFAULT_TIMEOUT_MS)]
    pub default_timeout_ms: u64,
    #[arg(long, default_value_t = DEFAULT_CAPTURE_LIMIT_BYTES)]
    pub capture_limit_bytes: usize,
    #[arg(long, help = "Print the mag manual (mag-how-to.md) and exit")]
    pub how: bool,
    #[arg(
        long,
        value_name = "MODE",
        default_value = "forward",
        help = "stdio lane: `forward` to the resident daemon (MAG_URL, default http://127.0.0.1:7734) or `local` private in-process engine"
    )]
    pub lane: String,
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    RunBatch {
        #[arg(long)]
        json: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(
            long,
            help = "Print the raw JSON response (default output is the humane [mag] rendering)"
        )]
        json_out: bool,
    },
    #[command(about = "Run named jobs via TOON script (see mag-how-to.md)")]
    RunToon {
        #[arg(long)]
        script: Option<String>,
        #[arg(long)]
        raw: Option<String>,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long)]
        session_id: Option<String>,
        #[arg(
            long,
            alias = "g0",
            help = "Disable token-diet filtering for this call"
        )]
        unfiltered: bool,
        #[arg(long, default_value_t = false)]
        close: bool,
    },
    #[command(about = "Open or control the shared tmux/WezTerm house terminal")]
    Term {
        #[command(subcommand)]
        command: TermCommand,
    },
}

#[derive(Subcommand, Debug)]
pub enum TermCommand {
    #[command(about = "Create the shared tmux session and open it in WezTerm")]
    Open {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(
            long,
            short = 'S',
            help = "tmux socket path (e.g. /tmp/tmux-1000/default); overrides server auto-detection"
        )]
        socket: Option<String>,
        #[arg(long, default_value_t = false)]
        no_wezterm: bool,
        #[arg(long, default_value_t = false)]
        no_watch: bool,
    },
    #[command(about = "Attach this terminal to the shared tmux session")]
    Attach {
        #[arg(long)]
        session: Option<String>,
        #[arg(long, short = 'S', help = "tmux socket path")]
        socket: Option<String>,
    },
    #[command(about = "Print shared terminal status")]
    Status {
        #[arg(long)]
        session: Option<String>,
        #[arg(long, short = 'S', help = "tmux socket path")]
        socket: Option<String>,
    },
    #[command(about = "Send input to the operator pane")]
    Send {
        #[arg(long)]
        session: Option<String>,
        input: String,
        #[arg(long, default_value_t = true)]
        enter: bool,
        #[arg(long, short = 'S', help = "tmux socket path")]
        socket: Option<String>,
    },
    #[command(about = "Print recent operator pane scrollback")]
    Snapshot {
        #[arg(long)]
        session: Option<String>,
        #[arg(long, default_value_t = 80)]
        lines: usize,
        #[arg(long, short = 'S', help = "tmux socket path")]
        socket: Option<String>,
    },
    #[command(about = "Close the shared tmux session")]
    Close {
        #[arg(long)]
        session: Option<String>,
        #[arg(long, short = 'S', help = "tmux socket path")]
        socket: Option<String>,
    },
}

/// Process exit status for a finished one-shot batch.
///
/// The rendered text always carries each job's own `exit=`; this is the single
/// number a shell's `$?` gets, so it answers *"did the work succeed"*, not
/// *"did mag dispatch"*. mag's own failures (bad JSON, unreadable file) keep
/// going out the `anyhow` path and stay exit 1 — untouched by this.
///
/// The first failing job in index order wins, so the single-command case (the
/// Bash-hook case) reports the wrapped command's own code verbatim. A code
/// outside `1..=255` — signal death (`-1`), timeout-less transport errors, or a
/// job that failed with an `error` set but a zero/absent code — collapses to 1:
/// a u8 exit status cannot carry those, and wrapping would turn a failure into
/// a success.
fn batch_exit_code(response: &BatchResponse) -> ExitCode {
    let Some(failed) = response.results.iter().find(|result| !result.success) else {
        return ExitCode::SUCCESS;
    };
    let code = failed
        .exit_code
        .filter(|code| (1..=255).contains(code))
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1);
    ExitCode::from(code)
}

pub async fn dispatch() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    if cli.how {
        print!("{}", include_str!("../mag-how-to.md"));
        return Ok(ExitCode::SUCCESS);
    }
    if cli.serve {
        if cli.command.is_some() {
            return Err(anyhow!("--serve cannot be combined with subcommands"));
        }
        crate::serve::run_serve_mode(&cli).await?;
        return Ok(ExitCode::SUCCESS);
    }

    // The stdio MCP lane (no subcommand) defaults to FORWARDING to the
    // resident daemon — that's the whole "every seat, every session, always
    // ~3ms" promise. `--lane local` keeps a private in-process engine for
    // debugging. CLI one-shots (run-batch/run-toon) always run a private
    // engine: they're smoke tests, not the resident lane.
    if cli.command.is_none() {
        let backend = match cli.lane.as_str() {
            "forward" => crate::mcp::ToolBackend::Forward {
                seat: crate::engine::local_seat(),
            },
            "local" => {
                let node_registry = load_node_registry(&cli)?;
                let engine = BatchEngine::new(EngineConfig {
                    workers: cli.workers.max(1),
                    default_timeout_ms: cli.default_timeout_ms,
                    capture_limit_bytes: cli.capture_limit_bytes.max(256),
                    node_registry,
                    token_diet_enabled: true,
                    token_diet_echo_max_bytes: crate::DEFAULT_DIET_ECHO_MAX_BYTES,
                })?;
                crate::mcp::ToolBackend::Local(engine)
            }
            other => {
                return Err(anyhow!(
                    "--lane must be `forward` or `local`, got `{}`",
                    other
                ));
            }
        };

        tracing::info!("starting {} {}", SERVER_NAME, SERVER_VERSION);
        let service = MagServer::new(backend)
            .serve(stdio())
            .await
            .inspect_err(|err| tracing::error!("serving error: {:?}", err))?;
        service.waiting().await?;
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(Commands::Term { command }) = cli.command {
        run_term(command)?;
        return Ok(ExitCode::SUCCESS);
    }

    let node_registry = load_node_registry(&cli)?;

    let engine = BatchEngine::new(EngineConfig {
        workers: cli.workers.max(1),
        default_timeout_ms: cli.default_timeout_ms,
        capture_limit_bytes: cli.capture_limit_bytes.max(256),
        node_registry,
        token_diet_enabled: true,
        token_diet_echo_max_bytes: crate::DEFAULT_DIET_ECHO_MAX_BYTES,
    })?;

    // The engine is dropped on the way out of this match, before the exit code
    // reaches `main` — that Drop is what reaps warm bash children
    // (`WarmShellWorker::drop`), so the code travels back as a value instead of
    // going out through `std::process::exit()` from inside the run functions.
    let code = match cli.command {
        Some(Commands::RunBatch {
            json,
            file,
            json_out,
        }) => run_local_batch(engine, json, file, json_out)?,
        Some(Commands::RunToon {
            script,
            raw,
            remote,
            file,
            session_id,
            unfiltered,
            close,
        }) => run_local_toon(
            engine,
            LocalToonArgs {
                script,
                raw,
                remote,
                file,
                session_id,
                unfiltered,
                close,
            },
        )?,
        Some(Commands::Term { .. }) => unreachable!("term handled before engine setup"),
        None => unreachable!("stdio lane handled above"),
    };

    Ok(code)
}

fn run_local_batch(
    engine: BatchEngine,
    json_arg: Option<String>,
    file: Option<PathBuf>,
    json_out: bool,
) -> AppResult<ExitCode> {
    let payload = if let Some(path) = file {
        fs::read_to_string(path)?
    } else if let Some(json) = json_arg {
        json
    } else {
        return Err(anyhow!("provide --json or --file"));
    };

    let request = serde_json::from_str::<BatchRequest>(&payload)?;
    let mut response = engine.run_batch(request);
    if json_out {
        println!("{}", serde_json::to_string_pretty(&response)?);
    } else {
        println!("{}", render_toon_response(&mut response));
    }
    Ok(batch_exit_code(&response))
}

struct LocalToonArgs {
    script: Option<String>,
    raw: Option<String>,
    remote: Option<String>,
    file: Option<PathBuf>,
    session_id: Option<String>,
    unfiltered: bool,
    close: bool,
}

fn run_local_toon(engine: BatchEngine, args: LocalToonArgs) -> AppResult<ExitCode> {
    let script = if let Some(path) = args.file {
        fs::read_to_string(path)?
    } else if let Some(raw) = args.raw {
        return render_local_toon(
            engine,
            ToonRequest {
                script: None,
                raw: Some(raw),
                remote: args.remote,
                session_id: args.session_id,
                sticky: None,
                close: Some(args.close),
                env: BTreeMap::new(),
                default_cwd: None,
                default_timeout_ms: None,
                max_parallel: None,
                continue_on_error: None,
                capture_limit_bytes: None,
                unfiltered: Some(args.unfiltered),
                warm: None,
            },
        );
    } else if let Some(script) = args.script {
        script
    } else if args.close && args.session_id.is_some() {
        return render_local_toon(
            engine,
            ToonRequest {
                script: None,
                raw: None,
                remote: args.remote,
                session_id: args.session_id,
                sticky: None,
                close: Some(true),
                env: BTreeMap::new(),
                default_cwd: None,
                default_timeout_ms: None,
                max_parallel: None,
                continue_on_error: None,
                capture_limit_bytes: None,
                unfiltered: Some(args.unfiltered),
                warm: None,
            },
        );
    } else {
        return Err(anyhow!("provide --script, --raw, or --file"));
    };

    render_local_toon(
        engine,
        ToonRequest {
            script: Some(script),
            raw: None,
            remote: args.remote,
            session_id: args.session_id,
            sticky: None,
            close: Some(args.close),
            env: BTreeMap::new(),
            default_cwd: None,
            default_timeout_ms: None,
            max_parallel: None,
            continue_on_error: None,
            capture_limit_bytes: None,
            unfiltered: Some(args.unfiltered),
            warm: None,
        },
    )
}

fn render_local_toon(engine: BatchEngine, request: ToonRequest) -> AppResult<ExitCode> {
    let mut response = engine.run_tool_request(request)?;
    println!("{}", render_toon_response(&mut response));
    Ok(batch_exit_code(&response))
}

fn run_term(command: TermCommand) -> AppResult<()> {
    match command {
        TermCommand::Open {
            session,
            cwd,
            socket,
            no_wezterm,
            no_watch,
        } => crate::term::open(&crate::term::TermOpenOptions {
            session,
            cwd,
            no_wezterm,
            no_watch,
            socket,
        }),
        TermCommand::Attach { session, socket } => {
            crate::term::attach(&crate::term::TermTargetOptions { session, socket })
        }
        TermCommand::Status { session, socket } => {
            crate::term::status(&crate::term::TermTargetOptions { session, socket })
        }
        TermCommand::Send {
            session,
            input,
            enter,
            socket,
        } => crate::term::send(&crate::term::TermSendOptions {
            session,
            input,
            enter,
            socket,
        }),
        TermCommand::Snapshot {
            session,
            lines,
            socket,
        } => crate::term::snapshot(&crate::term::TermSnapshotOptions {
            session,
            lines,
            socket,
        }),
        TermCommand::Close { session, socket } => {
            crate::term::close(&crate::term::TermTargetOptions { session, socket })
        }
    }
}
