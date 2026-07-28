use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_client::{
    CodexLoginStatus, LaunchResult, ProcessResult, TurnEvent, TurnResult, TurnStream,
};
use atra_protocol::{
    ApprovalId, ApprovalPolicy, CheckpointId, CommandMode, EventSequence, ProcessId, ThreadId,
};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::{EnvFilter, fmt::writer::BoxMakeWriter};

mod controller_client;
mod platform;
mod workspace;

use controller_client::{client, not_running as controller_not_running};
use workspace::ControllerStart;

#[derive(Parser)]
#[command(name = "atra")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Controller {
        #[command(subcommand)]
        command: ControllerCommand,
    },
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    Runner {
        #[command(subcommand)]
        command: RunnerCommand,
    },
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    Approval {
        #[command(subcommand)]
        command: ApprovalCommand,
    },
    Platform {
        #[command(subcommand)]
        command: PlatformCommand,
    },
    Codex {
        #[command(subcommand)]
        command: CodexCommand,
    },
    Tui,
}

#[derive(Subcommand)]
enum ControllerCommand {
    Start,
    Stop,
    Run,
    Status,
}

#[derive(Subcommand)]
enum WorkspaceCommand {
    Init,
    Start,
}

#[derive(Subcommand)]
enum CodexCommand {
    Login,
    Logout,
    Status,
}

#[derive(Subcommand)]
enum ThreadCommand {
    Create {
        #[arg(long)]
        name: Option<String>,
    },
    List,
    Rename {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        name: String,
    },
    Model {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        model: String,
        #[arg(long)]
        reasoning_effort: String,
    },
    Send {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        message: String,
    },
    Events {
        #[arg(long)]
        thread: i64,
    },
    Checkpoint {
        #[command(subcommand)]
        command: CheckpointCommand,
    },
    Fork {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        checkpoint: Option<i64>,
        #[arg(long)]
        sequence: i64,
        #[arg(long)]
        name: Option<String>,
    },
    Rewind {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        checkpoint: Option<i64>,
        #[arg(long)]
        sequence: i64,
    },
    Continue {
        #[arg(long)]
        thread: i64,
    },
}

#[derive(Subcommand)]
enum CheckpointCommand {
    Create {
        #[arg(long)]
        thread: i64,
    },
    List {
        #[arg(long)]
        thread: i64,
    },
    Events {
        #[arg(long)]
        checkpoint: i64,
    },
    Restore {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        checkpoint: i64,
    },
}

#[derive(Subcommand)]
enum ApprovalCommand {
    Allow {
        #[arg(long)]
        approval: u64,
    },
    Deny {
        #[arg(long)]
        approval: u64,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
enum PlatformCommand {
    Download,
    Install { bundle: PathBuf },
}

#[derive(Subcommand)]
enum RunnerCommand {
    Run {
        #[arg(long)]
        stdio: bool,
    },
    List,
    Upload {
        #[arg(long)]
        runner_binary: Option<PathBuf>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    Launch {
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: String,
        #[arg(long, value_enum)]
        approval: Approval,
        #[arg(last = true)]
        command: Vec<String>,
    },
    Exec {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        command: String,
        #[arg(long)]
        background: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
    },
    Wait {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_id: String,
        #[arg(long)]
        timeout_ms: u64,
    },
    Stop {
        #[arg(long)]
        thread: i64,
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_id: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Approval {
    Ask,
    Allow,
}

impl From<Approval> for ApprovalPolicy {
    fn from(value: Approval) -> Self {
        match value {
            Approval::Ask => Self::Ask,
            Approval::Allow => Self::Allow,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let command = Cli::parse().command;
    let log_writer = if matches!(&command, Command::Tui) {
        BoxMakeWriter::new(std::io::sink)
    } else {
        BoxMakeWriter::new(std::io::stderr)
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(log_writer)
        .compact()
        .init();

    if let Err(error) = run(command).await {
        eprintln!("atra: {error:#}");
        std::process::exit(1);
    }
}

async fn run(command: Command) -> Result<()> {
    if let Command::Runner {
        command: RunnerCommand::Run { stdio },
    } = &command
    {
        if !*stdio {
            bail!("--stdio is required");
        }
        return atra_runner::run_stdio().await;
    }
    match &command {
        Command::Platform {
            command: PlatformCommand::Download,
        } => return platform::download().await,
        Command::Platform {
            command: PlatformCommand::Install { bundle },
        } => return platform::install(bundle),
        _ => {}
    }
    let workspace = workspace::root()?;
    if matches!(
        &command,
        Command::Workspace {
            command: WorkspaceCommand::Init
        }
    ) {
        return workspace::init(&workspace);
    }
    let workspace_id = workspace::id(&workspace);
    let endpoint = workspace::endpoint(&workspace_id)?;

    match command {
        Command::Codex {
            command: CodexCommand::Login,
        } => {
            atra_controller::codex_login(&codex_auth_home()?).await?;
            println!("logged in");
            Ok(())
        }
        Command::Codex {
            command: CodexCommand::Logout,
        } => {
            match client(&endpoint).codex_logout().await {
                Ok(()) => {}
                Err(error) if controller_not_running(&error) => {
                    atra_controller::codex_logout(&codex_auth_home()?).await?;
                }
                Err(error) => return Err(error),
            }
            println!("logged out");
            Ok(())
        }
        Command::Codex {
            command: CodexCommand::Status,
        } => match client(&endpoint).codex_login_status().await? {
            CodexLoginStatus::LoggedIn { email } => {
                println!(
                    "logged in{}",
                    email
                        .map(|email| format!(" as {email}"))
                        .unwrap_or_default()
                );
                Ok(())
            }
            CodexLoginStatus::LoginRequired => {
                println!("logged out");
                Ok(())
            }
        },
        Command::Platform { .. } => {
            unreachable!("platform commands are handled before workspace setup")
        }
        Command::Controller {
            command: ControllerCommand::Start,
        } => {
            let database = workspace::database(&workspace_id)?;
            match workspace::start_controller(&workspace, &endpoint, &database).await? {
                ControllerStart::Started => println!("started"),
                ControllerStart::AlreadyRunning => println!("already running"),
            }
            Ok(())
        }
        Command::Controller {
            command: ControllerCommand::Stop,
        } => {
            workspace::stop_controller(&endpoint).await?;
            println!("stopped");
            Ok(())
        }
        Command::Controller {
            command: ControllerCommand::Run,
        } => {
            let database = workspace::database(&workspace_id)?;
            atra_controller::run(
                &endpoint,
                &database,
                &codex_auth_home()?,
                platform::current_platform()?,
            )
            .await
        }
        Command::Controller {
            command: ControllerCommand::Status,
        } => workspace::controller_status(&endpoint).await,
        Command::Workspace {
            command: WorkspaceCommand::Init,
        } => unreachable!("workspace init is handled before controller endpoint setup"),
        Command::Workspace {
            command: WorkspaceCommand::Start,
        } => workspace::start(&workspace, &endpoint, &workspace_id).await,
        Command::Thread { command } => run_thread(&endpoint, command).await,
        Command::Approval {
            command: ApprovalCommand::Allow { approval },
        } => display_turn_result(
            client(&endpoint)
                .approval_allow(ApprovalId(approval))
                .await?,
        ),
        Command::Approval {
            command: ApprovalCommand::Deny { approval, reason },
        } => display_turn_result(
            client(&endpoint)
                .approval_deny(ApprovalId(approval), reason)
                .await?,
        ),
        Command::Runner {
            command: RunnerCommand::Run { .. },
        } => unreachable!("runner run is handled before workspace setup"),
        Command::Runner {
            command: RunnerCommand::List,
        } => {
            for runner in client(&endpoint).runner_list().await? {
                println!("{}\t{}", runner.name, runner.description);
            }
            Ok(())
        }
        Command::Runner {
            command:
                RunnerCommand::Upload {
                    runner_binary,
                    command,
                },
        } => platform::upload_runner(runner_binary, command).await,
        Command::Runner {
            command:
                RunnerCommand::Launch {
                    name,
                    description,
                    approval,
                    command,
                },
        } => {
            let command = runner_command(command)?;
            match client(&endpoint)
                .runner_launch(name, description, approval.into(), command)
                .await?
            {
                LaunchResult::Launched => println!("launched"),
                LaunchResult::AlreadyRunning => println!("already running"),
            }
            Ok(())
        }
        Command::Runner {
            command:
                RunnerCommand::Exec {
                    thread,
                    name,
                    command,
                    background,
                    timeout_ms,
                },
        } => {
            let mode = match (background, timeout_ms) {
                (true, None) => CommandMode::Background,
                (false, timeout_ms) => CommandMode::Foreground { timeout_ms },
                (true, Some(_)) => bail!("background commands cannot have timeout options"),
            };
            display_process_result(
                client(&endpoint)
                    .exec_command(ThreadId(thread), name, command, mode)
                    .await?,
            )
        }
        Command::Tui => {
            if workspace::prepare_tui(&workspace, &endpoint, &workspace_id).await? {
                let database = workspace::database(&workspace_id)?;
                let message_history = database.with_file_name("tui-history.jsonl");
                let command_history = database.with_file_name("tui-command-history.jsonl");
                atra_tui::run(endpoint, message_history, command_history).await
            } else {
                Ok(())
            }
        }
        Command::Runner {
            command:
                RunnerCommand::Wait {
                    thread,
                    name,
                    process_id,
                    timeout_ms,
                },
        } => display_process_result(
            client(&endpoint)
                .wait_process(ThreadId(thread), name, ProcessId(process_id), timeout_ms)
                .await?,
        ),
        Command::Runner {
            command:
                RunnerCommand::Stop {
                    thread,
                    name,
                    process_id,
                },
        } => display_process_result(
            client(&endpoint)
                .stop_process(ThreadId(thread), name, ProcessId(process_id))
                .await?,
        ),
    }
}

async fn run_thread(endpoint: &Path, command: ThreadCommand) -> Result<()> {
    match command {
        ThreadCommand::Create { name } => {
            println!("{}", client(endpoint).thread_create(name).await?);
            Ok(())
        }
        ThreadCommand::List => {
            for thread in client(endpoint).thread_list().await? {
                println!(
                    "{}\t{}",
                    thread.id,
                    thread.display_name.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        ThreadCommand::Rename { thread, name } => {
            client(endpoint)
                .thread_rename(ThreadId(thread), name)
                .await?;
            println!("renamed");
            Ok(())
        }
        ThreadCommand::Model {
            thread,
            model,
            reasoning_effort,
        } => {
            client(endpoint)
                .thread_set_model(ThreadId(thread), model, reasoning_effort)
                .await?;
            println!("model changed");
            Ok(())
        }
        ThreadCommand::Send { thread, message } => {
            display_turn_stream(
                client(endpoint)
                    .thread_send(ThreadId(thread), message)
                    .await?,
            )
            .await
        }
        ThreadCommand::Events { thread } => {
            for event in client(endpoint).thread_events(ThreadId(thread)).await? {
                println!(
                    "{}",
                    serde_json::to_string(&event).context("failed to encode thread event")?
                );
            }
            Ok(())
        }
        ThreadCommand::Checkpoint { command } => run_checkpoint(endpoint, command).await,
        ThreadCommand::Fork {
            thread,
            checkpoint,
            sequence,
            name,
        } => {
            println!(
                "{}",
                client(endpoint)
                    .thread_fork(
                        ThreadId(thread),
                        checkpoint.map(CheckpointId),
                        EventSequence(sequence),
                        name,
                    )
                    .await?
            );
            Ok(())
        }
        ThreadCommand::Rewind {
            thread,
            checkpoint,
            sequence,
        } => {
            client(endpoint)
                .thread_rewind(
                    ThreadId(thread),
                    checkpoint.map(CheckpointId),
                    EventSequence(sequence),
                )
                .await?;
            println!("rewound");
            Ok(())
        }
        ThreadCommand::Continue { thread } => {
            display_turn_stream(client(endpoint).thread_continue(ThreadId(thread)).await?).await
        }
    }
}

async fn run_checkpoint(endpoint: &Path, command: CheckpointCommand) -> Result<()> {
    match command {
        CheckpointCommand::Create { thread } => {
            println!(
                "{}",
                client(endpoint).checkpoint_create(ThreadId(thread)).await?
            );
            Ok(())
        }
        CheckpointCommand::List { thread } => {
            for checkpoint in client(endpoint).checkpoint_list(ThreadId(thread)).await? {
                println!(
                    "{}\t{}\t{}",
                    checkpoint.id, checkpoint.created_at_ms, checkpoint.reason
                );
            }
            Ok(())
        }
        CheckpointCommand::Events { checkpoint } => {
            for event in client(endpoint)
                .checkpoint_events(CheckpointId(checkpoint))
                .await?
            {
                println!(
                    "{}",
                    serde_json::to_string(&event).context("failed to encode checkpoint event")?
                );
            }
            Ok(())
        }
        CheckpointCommand::Restore { thread, checkpoint } => {
            client(endpoint)
                .checkpoint_restore(ThreadId(thread), CheckpointId(checkpoint))
                .await?;
            println!("restored");
            Ok(())
        }
    }
}

fn display_turn_result(result: TurnResult) -> Result<()> {
    match result {
        TurnResult::ApprovalResolved => Ok(()),
        TurnResult::Cancelled => {
            println!("cancelled");
            Ok(())
        }
        TurnResult::Completed { content } => {
            println!("{content}");
            Ok(())
        }
        TurnResult::ApprovalRequired {
            approval_id,
            tool,
            arguments,
        } => {
            println!("{approval_id}");
            println!("tool: {tool}");
            println!(
                "arguments: {}",
                serde_json::to_string(&arguments).context("failed to encode tool arguments")?
            );
            Ok(())
        }
    }
}

async fn display_turn_stream(mut stream: TurnStream) -> Result<()> {
    loop {
        match stream.receive().await?.event {
            TurnEvent::Started
            | TurnEvent::Delta { .. }
            | TurnEvent::ReasoningSummaryDelta { .. }
            | TurnEvent::ReasoningSummaryPartAdded
            | TurnEvent::ToolCallStarted { .. }
            | TurnEvent::ToolCallDelta { .. }
            | TurnEvent::RunnerOperation { .. }
            | TurnEvent::Event { .. } => {}
            TurnEvent::ApprovalRequired {
                approval_id,
                tool,
                arguments,
                ..
            } => {
                println!("{approval_id}");
                println!("tool: {tool}");
                println!(
                    "arguments: {}",
                    serde_json::to_string(&arguments).context("failed to encode tool arguments")?
                );
                std::io::stdout()
                    .flush()
                    .context("failed to flush approval request")?;
            }
            TurnEvent::Finished(result) => return display_turn_result(result),
        }
    }
}

fn display_process_result(result: ProcessResult) -> Result<()> {
    match result {
        ProcessResult::Started { process_id } => {
            println!("{process_id}");
            Ok(())
        }
        ProcessResult::Running { process_id, output } => {
            print!("{output}");
            eprintln!("process \"{process_id}\" is still running");
            Ok(())
        }
        ProcessResult::Finished { output, exit_code } => {
            print!("{output}");
            if exit_code != Some(0) {
                bail!(
                    "command exited with {}",
                    exit_code
                        .map(|code| format!("status {code}"))
                        .unwrap_or_else(|| "a signal".to_owned())
                );
            }
            Ok(())
        }
        ProcessResult::Stopped { output } => {
            print!("{output}");
            Ok(())
        }
    }
}

fn runner_command(command: Vec<String>) -> Result<Vec<String>> {
    if !command.is_empty() {
        return Ok(command);
    }

    let binary = env::current_exe().context("failed to determine the atra executable path")?;
    Ok(vec![
        binary
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("atra executable path is not valid UTF-8"))?,
        "runner".to_owned(),
        "run".to_owned(),
        "--stdio".to_owned(),
    ])
}

fn codex_auth_home() -> Result<PathBuf> {
    Ok(xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")?
        .join("atra/codex"))
}
