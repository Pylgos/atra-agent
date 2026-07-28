use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ApprovalPolicy, CommandMode, ControllerRequest, ControllerResponse};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::{EnvFilter, fmt::writer::BoxMakeWriter};

mod controller_client;
mod platform;
mod workspace;

use controller_client::{
    not_running as controller_not_running, request as send_controller_request,
};
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
        name: String,
        #[arg(long)]
        command: String,
        #[arg(long)]
        background: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long, value_enum, default_value_t = OnTimeout::ReturnRunning)]
        on_timeout: OnTimeout,
    },
    Wait {
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_handle: String,
        #[arg(long)]
        timeout_ms: u64,
    },
    Stop {
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_handle: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Approval {
    Ask,
    Allow,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnTimeout {
    ReturnRunning,
    Terminate,
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
            match send_controller_request(&endpoint, ControllerRequest::CodexLogout).await {
                Ok(ControllerResponse::CodexLoggedOut) => {}
                Ok(ControllerResponse::Error { message }) => bail!("{message}"),
                Ok(response) => {
                    bail!("controller returned an unexpected response: {response:?}")
                }
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
        } => match send_controller_request(&endpoint, ControllerRequest::CodexLoginStatus).await? {
            ControllerResponse::CodexLoggedIn { email } => {
                println!(
                    "logged in{}",
                    email
                        .map(|email| format!(" as {email}"))
                        .unwrap_or_default()
                );
                Ok(())
            }
            ControllerResponse::CodexLoginRequired => {
                println!("logged out");
                Ok(())
            }
            ControllerResponse::Error { message } => bail!("{message}"),
            response => bail!("controller returned an unexpected response: {response:?}"),
        },
        Command::Platform {
            command: PlatformCommand::Install { bundle },
        } => platform::install(&bundle),
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
            atra_controller::run(&endpoint, &database, &codex_auth_home()?).await
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
        Command::Thread {
            command: ThreadCommand::Create { name },
        } => {
            match send_controller_request(
                &endpoint,
                ControllerRequest::ThreadCreate { display_name: name },
            )
            .await?
            {
                ControllerResponse::ThreadCreated { thread_id } => println!("{thread_id}"),
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
            }
            Ok(())
        }
        Command::Thread {
            command: ThreadCommand::List,
        } => {
            match send_controller_request(&endpoint, ControllerRequest::ThreadList).await? {
                ControllerResponse::ThreadList { threads } => {
                    for thread in threads {
                        println!(
                            "{}\t{}",
                            thread.id,
                            thread.display_name.as_deref().unwrap_or("")
                        );
                    }
                }
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
            }
            Ok(())
        }
        Command::Thread {
            command: ThreadCommand::Rename { thread, name },
        } => {
            controller_request(
                &endpoint,
                ControllerRequest::ThreadRename {
                    thread_id: thread,
                    display_name: name,
                },
            )
            .await
        }
        Command::Thread {
            command:
                ThreadCommand::Model {
                    thread,
                    model,
                    reasoning_effort,
                },
        } => {
            controller_request(
                &endpoint,
                ControllerRequest::ThreadSetModel {
                    thread_id: thread,
                    model,
                    reasoning_effort,
                },
            )
            .await
        }
        Command::Thread {
            command: ThreadCommand::Send { thread, message },
        } => {
            display_turn_stream(
                &endpoint,
                ControllerRequest::ThreadSend {
                    thread_id: thread,
                    message,
                },
            )
            .await
        }
        Command::Thread {
            command: ThreadCommand::Events { thread },
        } => {
            match send_controller_request(
                &endpoint,
                ControllerRequest::ThreadEvents { thread_id: thread },
            )
            .await?
            {
                ControllerResponse::ThreadEvents { events } => {
                    for event in events {
                        println!(
                            "{}",
                            serde_json::to_string(&event)
                                .context("failed to encode thread event")?
                        );
                    }
                }
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
            }
            Ok(())
        }
        Command::Thread {
            command: ThreadCommand::Checkpoint { command },
        } => match command {
            CheckpointCommand::Create { thread } => {
                match send_controller_request(
                    &endpoint,
                    ControllerRequest::ThreadCheckpointCreate { thread_id: thread },
                )
                .await?
                {
                    ControllerResponse::ThreadCheckpointCreated { checkpoint_id } => {
                        println!("{checkpoint_id}");
                        Ok(())
                    }
                    ControllerResponse::Error { message } => bail!("{message}"),
                    response => {
                        bail!("controller returned an unexpected response: {response:?}")
                    }
                }
            }
            CheckpointCommand::List { thread } => {
                match send_controller_request(
                    &endpoint,
                    ControllerRequest::ThreadCheckpointList { thread_id: thread },
                )
                .await?
                {
                    ControllerResponse::ThreadCheckpointList { checkpoints } => {
                        for checkpoint in checkpoints {
                            println!(
                                "{}\t{}\t{}",
                                checkpoint.id, checkpoint.created_at_ms, checkpoint.reason
                            );
                        }
                        Ok(())
                    }
                    ControllerResponse::Error { message } => bail!("{message}"),
                    response => {
                        bail!("controller returned an unexpected response: {response:?}")
                    }
                }
            }
            CheckpointCommand::Events { checkpoint } => {
                match send_controller_request(
                    &endpoint,
                    ControllerRequest::ThreadCheckpointEvents {
                        checkpoint_id: checkpoint,
                    },
                )
                .await?
                {
                    ControllerResponse::ThreadCheckpointEvents { events } => {
                        for event in events {
                            println!(
                                "{}",
                                serde_json::to_string(&event)
                                    .context("failed to encode checkpoint event")?
                            );
                        }
                        Ok(())
                    }
                    ControllerResponse::Error { message } => bail!("{message}"),
                    response => {
                        bail!("controller returned an unexpected response: {response:?}")
                    }
                }
            }
            CheckpointCommand::Restore { thread, checkpoint } => {
                controller_request(
                    &endpoint,
                    ControllerRequest::ThreadCheckpointRestore {
                        thread_id: thread,
                        checkpoint_id: checkpoint,
                    },
                )
                .await
            }
        },
        Command::Thread {
            command:
                ThreadCommand::Fork {
                    thread,
                    checkpoint,
                    sequence,
                    name,
                },
        } => {
            match send_controller_request(
                &endpoint,
                ControllerRequest::ThreadFork {
                    thread_id: thread,
                    checkpoint_id: checkpoint,
                    sequence,
                    display_name: name,
                },
            )
            .await?
            {
                ControllerResponse::ThreadForked { thread_id } => println!("{thread_id}"),
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
            }
            Ok(())
        }
        Command::Thread {
            command:
                ThreadCommand::Rewind {
                    thread,
                    checkpoint,
                    sequence,
                },
        } => {
            controller_request(
                &endpoint,
                ControllerRequest::ThreadRewind {
                    thread_id: thread,
                    checkpoint_id: checkpoint,
                    sequence,
                },
            )
            .await
        }
        Command::Thread {
            command: ThreadCommand::Continue { thread },
        } => {
            display_turn_stream(
                &endpoint,
                ControllerRequest::ThreadContinue { thread_id: thread },
            )
            .await
        }
        Command::Approval {
            command: ApprovalCommand::Allow { approval },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::ApprovalAllow {
                    approval_id: approval,
                },
            )
            .await?;
            display_turn_response(response)
        }
        Command::Approval {
            command: ApprovalCommand::Deny { approval, reason },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::ApprovalDeny {
                    approval_id: approval,
                    reason,
                },
            )
            .await?;
            display_turn_response(response)
        }
        Command::Runner {
            command: RunnerCommand::Run { .. },
        } => unreachable!("runner run is handled before workspace setup"),
        Command::Runner {
            command: RunnerCommand::List,
        } => {
            match send_controller_request(&endpoint, ControllerRequest::RunnerList).await? {
                ControllerResponse::RunnerList { runners } => {
                    for runner in runners {
                        println!("{}\t{}", runner.name, runner.description);
                    }
                }
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
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
            controller_request(
                &endpoint,
                ControllerRequest::RunnerLaunch {
                    name,
                    description,
                    approval: approval.into(),
                    command,
                },
            )
            .await
        }
        Command::Runner {
            command:
                RunnerCommand::Exec {
                    name,
                    command,
                    background,
                    timeout_ms,
                    on_timeout,
                },
        } => {
            let mode = match (background, timeout_ms, on_timeout) {
                (true, None, OnTimeout::ReturnRunning) => CommandMode::Background,
                (false, timeout_ms, OnTimeout::ReturnRunning) => {
                    CommandMode::Foreground { timeout_ms }
                }
                (false, Some(timeout_ms), OnTimeout::Terminate) => {
                    CommandMode::Timed { timeout_ms }
                }
                (true, _, _) => bail!("background commands cannot have timeout options"),
                (false, None, OnTimeout::Terminate) => {
                    bail!("--on-timeout terminate requires --timeout-ms")
                }
            };
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::ExecCommand {
                    runner: name,
                    command,
                    mode,
                },
            )
            .await?;
            display_process_response(response)
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
                    name,
                    process_handle,
                    timeout_ms,
                },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::WaitProcess {
                    runner: name,
                    process_handle,
                    timeout_ms,
                },
            )
            .await?;
            display_process_response(response)
        }
        Command::Runner {
            command:
                RunnerCommand::Stop {
                    name,
                    process_handle,
                },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::StopProcess {
                    runner: name,
                    process_handle,
                },
            )
            .await?;
            display_process_response(response)
        }
    }
}

fn display_turn_response(response: ControllerResponse) -> Result<()> {
    match response {
        ControllerResponse::ApprovalResolved => Ok(()),
        ControllerResponse::ThreadCancelled => {
            println!("cancelled");
            Ok(())
        }
        ControllerResponse::TurnCompleted { content } => {
            println!("{content}");
            Ok(())
        }
        ControllerResponse::ApprovalRequired {
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
            Ok(())
        }
        ControllerResponse::Error { message } => bail!("{message}"),
        response => bail!("controller returned an unexpected response: {response:?}"),
    }
}

async fn display_turn_stream(endpoint: &Path, request: ControllerRequest) -> Result<()> {
    let mut connection = atra_client::Connection::open(endpoint, &request).await?;
    loop {
        match connection.receive().await? {
            ControllerResponse::TurnStarted { .. }
            | ControllerResponse::TurnDelta { .. }
            | ControllerResponse::ReasoningSummaryDelta { .. }
            | ControllerResponse::ReasoningSummaryPartAdded
            | ControllerResponse::ToolCallStarted { .. }
            | ControllerResponse::ToolCallDelta { .. }
            | ControllerResponse::RunnerOperationUpdate { .. }
            | ControllerResponse::TurnEvent { .. } => {}
            ControllerResponse::ApprovalRequired {
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
            response => return display_turn_response(response),
        }
    }
}

fn display_process_response(response: ControllerResponse) -> Result<()> {
    match response {
        ControllerResponse::ProcessStarted { process_handle } => {
            println!("{process_handle}");
            Ok(())
        }
        ControllerResponse::ProcessRunning {
            process_handle,
            output,
        } => {
            print!("{output}");
            eprintln!("process {process_handle:?} is still running");
            Ok(())
        }
        ControllerResponse::ProcessFinished { output, exit_code } => {
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
        ControllerResponse::ProcessTimedOut { output } => {
            print!("{output}");
            bail!("command timed out")
        }
        ControllerResponse::ProcessStopped { output } => {
            print!("{output}");
            Ok(())
        }
        ControllerResponse::Error { message } => bail!("{message}"),
        response => bail!("controller returned an unexpected response: {response:?}"),
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

async fn controller_request(endpoint: &Path, request: ControllerRequest) -> Result<()> {
    let response = send_controller_request(endpoint, request).await?;
    match response {
        ControllerResponse::Running => println!("running"),
        ControllerResponse::Stopping => println!("stopping"),
        ControllerResponse::ThreadRenamed => println!("renamed"),
        ControllerResponse::ThreadModelChanged => println!("model changed"),
        ControllerResponse::ThreadCheckpointRestored => println!("restored"),
        ControllerResponse::ThreadRewound => println!("rewound"),
        ControllerResponse::ThreadCreated { .. }
        | ControllerResponse::ThreadList { .. }
        | ControllerResponse::ModelList { .. }
        | ControllerResponse::TurnStarted { .. }
        | ControllerResponse::TurnDelta { .. }
        | ControllerResponse::ReasoningSummaryDelta { .. }
        | ControllerResponse::ReasoningSummaryPartAdded
        | ControllerResponse::ToolCallStarted { .. }
        | ControllerResponse::ToolCallDelta { .. }
        | ControllerResponse::TurnEvent { .. }
        | ControllerResponse::TurnCompleted { .. }
        | ControllerResponse::ThreadCancelled
        | ControllerResponse::ThreadNotActive
        | ControllerResponse::ThreadProcessList { .. }
        | ControllerResponse::ThreadProcessInspect { .. }
        | ControllerResponse::ThreadProcessStopped
        | ControllerResponse::ApprovalResolved
        | ControllerResponse::ApprovalRequired { .. }
        | ControllerResponse::RunnerOperationUpdate { .. }
        | ControllerResponse::ThreadEvents { .. }
        | ControllerResponse::ThreadCheckpointCreated { .. }
        | ControllerResponse::ThreadCheckpointList { .. }
        | ControllerResponse::ThreadCheckpointEvents { .. }
        | ControllerResponse::ThreadForked { .. }
        | ControllerResponse::RunnerList { .. }
        | ControllerResponse::CodexLoginRequired
        | ControllerResponse::CodexLoggedIn { .. }
        | ControllerResponse::CodexLoggedOut => {
            bail!("controller returned an unexpected thread response")
        }
        ControllerResponse::Launched => println!("launched"),
        ControllerResponse::AlreadyRunning => println!("already running"),
        ControllerResponse::ProcessStarted { .. }
        | ControllerResponse::ProcessRunning { .. }
        | ControllerResponse::ProcessFinished { .. }
        | ControllerResponse::ProcessTimedOut { .. }
        | ControllerResponse::ProcessStopped { .. } => {
            bail!("controller returned an unexpected process response")
        }
        ControllerResponse::Error { message } => bail!("{message}"),
    }
    Ok(())
}
