use std::{
    env, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ApprovalPolicy, ControllerRequest, ControllerResponse, TimeoutAction};
use clap::{Parser, Subcommand, ValueEnum};
use rustix::process::getuid;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};
use tracing_subscriber::EnvFilter;

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
    Tui,
}

#[derive(Subcommand)]
enum ControllerCommand {
    Run,
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
enum RunnerCommand {
    Launch {
        #[arg(long)]
        name: String,
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
        cwd: Option<String>,
        #[arg(long)]
        background: bool,
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(long, value_enum, default_value_t = OnTimeout::ReturnRunning)]
        on_timeout: OnTimeout,
    },
    ApplyPatch {
        #[arg(long)]
        name: String,
        #[arg(long)]
        cwd: Option<String>,
    },
    Wait {
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_handle: u64,
        #[arg(long)]
        timeout_ms: u64,
    },
    Write {
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_handle: u64,
        #[arg(long)]
        text: String,
    },
    Stop {
        #[arg(long)]
        name: String,
        #[arg(long)]
        process_handle: u64,
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

impl From<OnTimeout> for TimeoutAction {
    fn from(value: OnTimeout) -> Self {
        match value {
            OnTimeout::ReturnRunning => Self::ReturnRunning,
            OnTimeout::Terminate => Self::Terminate,
        }
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .with_writer(std::io::stderr)
        .compact()
        .init();

    if let Err(error) = run().await {
        eprintln!("atra: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let command = Cli::parse().command;
    let workspace_id = workspace_id()?;
    let endpoint = controller_endpoint(&workspace_id)?;

    match command {
        Command::Controller {
            command: ControllerCommand::Run,
        } => {
            let database = controller_database(&workspace_id)?;
            atra_controller::run(&endpoint, &database).await
        }
        Command::Controller {
            command: ControllerCommand::Status,
        } => controller_status(&endpoint).await,
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
            command: ThreadCommand::Send { thread, message },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::ThreadSend {
                    thread_id: thread,
                    message,
                },
            )
            .await?;
            display_turn_response(response)
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
            command:
                RunnerCommand::Launch {
                    name,
                    approval,
                    command,
                },
        } => {
            let command = runner_command(command)?;
            controller_request(
                &endpoint,
                ControllerRequest::RunnerLaunch {
                    name,
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
                    cwd,
                    background,
                    timeout_ms,
                    on_timeout,
                },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::ExecCommand {
                    runner: name,
                    command,
                    cwd,
                    background,
                    timeout_ms,
                    timeout_action: on_timeout.into(),
                },
            )
            .await?;
            display_process_response(response)
        }
        Command::Tui => atra_tui::run(endpoint).await,
        Command::Runner {
            command: RunnerCommand::ApplyPatch { name, cwd },
        } => {
            let mut patch = String::new();
            tokio::io::stdin()
                .read_to_string(&mut patch)
                .await
                .context("failed to read patch from stdin")?;
            match send_controller_request(
                &endpoint,
                ControllerRequest::ApplyPatch {
                    runner: name,
                    patch,
                    cwd,
                },
            )
            .await?
            {
                ControllerResponse::PatchApplied { output } => {
                    print!("{output}");
                    Ok(())
                }
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
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
                RunnerCommand::Write {
                    name,
                    process_handle,
                    text,
                },
        } => {
            let response = send_controller_request(
                &endpoint,
                ControllerRequest::WriteProcess {
                    runner: name,
                    process_handle,
                    input: text.into_bytes(),
                },
            )
            .await?;
            match response {
                ControllerResponse::InputWritten => Ok(()),
                ControllerResponse::Error { message } => bail!("{message}"),
                response => bail!("controller returned an unexpected response: {response:?}"),
            }
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
            eprintln!("process {process_handle} is still running");
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

    let binary = match env::var("ATRA_RUNNER_BINARY") {
        Ok(binary) => PathBuf::from(binary),
        Err(env::VarError::NotPresent) => {
            let executable =
                env::current_exe().context("failed to determine the atra executable path")?;
            executable
                .parent()
                .context("atra executable has no parent directory")?
                .join("atra-runner")
        }
        Err(error) => return Err(error).context("ATRA_RUNNER_BINARY is not valid UTF-8"),
    };
    Ok(vec![
        binary
            .into_os_string()
            .into_string()
            .map_err(|_| anyhow::anyhow!("runner binary path is not valid UTF-8"))?,
        "--stdio".to_owned(),
    ])
}

fn workspace_id() -> Result<String> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let cwd = fs::canonicalize(&cwd)
        .with_context(|| format!("failed to resolve workspace directory {}", cwd.display()))?;
    Ok(format!("{:x}", Sha256::digest(cwd.as_os_str().as_encoded_bytes()))[..16].to_owned())
}

fn controller_endpoint(workspace_id: &str) -> Result<PathBuf> {
    if let Some(endpoint) = env::var_os("ATRA_CONTROLLER_ENDPOINT") {
        return Ok(PathBuf::from(endpoint));
    }

    let runtime_dir = match xdg::BaseDirectories::new().get_runtime_directory() {
        Ok(path) => path.join("atra"),
        Err(_) => PathBuf::from(format!("/tmp/atra-{}", getuid().as_raw())),
    };

    ensure_private_directory(&runtime_dir)?;
    let workspace_dir = runtime_dir.join(workspace_id);
    ensure_private_directory(&workspace_dir)?;
    Ok(workspace_dir.join("controller.sock"))
}

fn controller_database(workspace_id: &str) -> Result<PathBuf> {
    if let Some(database) = env::var_os("ATRA_CONTROLLER_STATE") {
        return Ok(PathBuf::from(database));
    }

    let state_home = xdg::BaseDirectories::new()
        .get_state_home()
        .context("cannot determine the XDG state directory")?;
    fs::create_dir_all(&state_home)
        .with_context(|| format!("failed to create state directory {}", state_home.display()))?;
    let atra_dir = state_home.join("atra");
    ensure_private_directory(&atra_dir)?;
    let workspace_dir = atra_dir.join(workspace_id);
    ensure_private_directory(&workspace_dir)?;
    Ok(workspace_dir.join("controller.sqlite3"))
}

fn ensure_private_directory(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("failed to set permissions on {}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to create directory {}", path.display()));
        }
    }

    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("failed to inspect directory {}", path.display()))?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        bail!(
            "{} must be a directory owned by the current user with mode 0700",
            path.display()
        );
    }
    Ok(())
}

async fn controller_status(endpoint: &Path) -> Result<()> {
    controller_request(endpoint, ControllerRequest::Status).await
}

async fn controller_request(endpoint: &Path, request: ControllerRequest) -> Result<()> {
    let is_status = matches!(request, ControllerRequest::Status);
    let response = send_controller_request(endpoint, request).await;
    let response = match response {
        Ok(response) => response,
        Err(error)
            if is_status
                && error.downcast_ref::<std::io::Error>().is_some_and(|error| {
                    matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    )
                }) =>
        {
            println!("stopped");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match response {
        ControllerResponse::Running => println!("running"),
        ControllerResponse::ThreadRenamed => println!("renamed"),
        ControllerResponse::ThreadCreated { .. }
        | ControllerResponse::ThreadList { .. }
        | ControllerResponse::TurnCompleted { .. }
        | ControllerResponse::ApprovalRequired { .. }
        | ControllerResponse::ThreadEvents { .. } => {
            bail!("controller returned an unexpected thread response")
        }
        ControllerResponse::Launched => println!("launched"),
        ControllerResponse::AlreadyRunning => println!("already running"),
        ControllerResponse::ProcessStarted { .. }
        | ControllerResponse::ProcessRunning { .. }
        | ControllerResponse::ProcessFinished { .. }
        | ControllerResponse::ProcessTimedOut { .. }
        | ControllerResponse::InputWritten
        | ControllerResponse::ProcessStopped { .. }
        | ControllerResponse::PatchApplied { .. } => {
            bail!("controller returned an unexpected process response")
        }
        ControllerResponse::Error { message } => bail!("{message}"),
    }
    Ok(())
}

async fn send_controller_request(
    endpoint: &Path,
    request: ControllerRequest,
) -> Result<ControllerResponse> {
    let mut stream = match UnixStream::connect(endpoint).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(error).context("controller is not running");
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to connect to controller at {}", endpoint.display())
            });
        }
    };

    let mut request =
        serde_json::to_vec(&request).context("failed to encode controller request")?;
    request.push(b'\n');
    stream
        .write_all(&request)
        .await
        .context("failed to write controller request")?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .context("failed to read controller response")?;
    let response: ControllerResponse =
        serde_json::from_str(&response).context("failed to decode controller response")?;
    Ok(response)
}
