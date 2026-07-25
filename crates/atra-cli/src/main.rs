use std::{
    env, fs,
    io::{IsTerminal, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ApprovalPolicy, ControllerRequest, ControllerResponse, TimeoutAction};
use clap::{Parser, Subcommand, ValueEnum};
use rustix::process::getuid;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command as TokioCommand,
    time::{Instant, sleep},
};
use tracing_subscriber::{EnvFilter, fmt::writer::BoxMakeWriter};

mod controller_client;
mod platform;

use controller_client::{
    not_running as controller_not_running, request as send_controller_request,
};

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
    let workspace = workspace_root()?;
    if matches!(
        &command,
        Command::Workspace {
            command: WorkspaceCommand::Init
        }
    ) {
        return workspace_init(&workspace);
    }
    let workspace_id = workspace_id(&workspace);
    let endpoint = controller_endpoint(&workspace_id)?;

    match command {
        Command::Codex {
            command: CodexCommand::Login,
        } => {
            atra_controller::codex_login(&codex_auth_home()?).await?;
            println!("logged in");
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
            let database = controller_database(&workspace_id)?;
            match controller_start(&workspace, &endpoint, &database).await? {
                ControllerStart::Started => println!("started"),
                ControllerStart::AlreadyRunning => println!("already running"),
            }
            Ok(())
        }
        Command::Controller {
            command: ControllerCommand::Stop,
        } => {
            controller_stop(&endpoint).await?;
            println!("stopped");
            Ok(())
        }
        Command::Controller {
            command: ControllerCommand::Run,
        } => {
            let database = controller_database(&workspace_id)?;
            atra_controller::run(&endpoint, &database, &codex_auth_home()?).await
        }
        Command::Controller {
            command: ControllerCommand::Status,
        } => controller_status(&endpoint).await,
        Command::Workspace {
            command: WorkspaceCommand::Init,
        } => unreachable!("workspace init is handled before controller endpoint setup"),
        Command::Workspace {
            command: WorkspaceCommand::Start,
        } => workspace_start(&workspace, &endpoint, &workspace_id).await,
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
        Command::Tui => {
            if prepare_tui(&workspace, &endpoint, &workspace_id).await? {
                let history =
                    controller_database(&workspace_id)?.with_file_name("tui-history.jsonl");
                atra_tui::run(endpoint, history).await
            } else {
                Ok(())
            }
        }
        Command::Runner {
            command: RunnerCommand::ApplyPatch { name },
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

const WORKSPACE_CONFIG: &str = ".config/atra.toml";
const WORKSPACE_SETUP: &str = ".config/atra-setup.bash";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfig {
    setup: String,
}

#[derive(Clone, Copy)]
enum ControllerStart {
    Started,
    AlreadyRunning,
}

fn workspace_root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    fs::canonicalize(&cwd)
        .with_context(|| format!("failed to resolve workspace directory {}", cwd.display()))
}

fn workspace_id(workspace: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(workspace.as_os_str().as_encoded_bytes())
    )[..16]
        .to_owned()
}

fn workspace_init(workspace: &Path) -> Result<()> {
    let config_path = workspace.join(WORKSPACE_CONFIG);
    let setup_path = workspace.join(WORKSPACE_SETUP);
    if config_path.exists() {
        bail!(
            "workspace is already initialized at {}",
            config_path.display()
        );
    }
    if setup_path.exists() {
        bail!("refusing to overwrite {}", setup_path.display());
    }

    let config_directory = config_path
        .parent()
        .expect("workspace config path should have a parent");
    fs::create_dir_all(config_directory).with_context(|| {
        format!(
            "failed to create workspace config directory {}",
            config_directory.display()
        )
    })?;
    fs::write(
        &config_path,
        format!("setup = \"bash {WORKSPACE_SETUP}\"\n"),
    )
    .with_context(|| format!("failed to write workspace config {}", config_path.display()))?;
    fs::write(
        &setup_path,
        concat!(
            "#!/usr/bin/env bash\n",
            "set -euo pipefail\n",
            "\n",
            "\"${ATRA_BINARY:-atra}\" runner launch \\\n",
            "  --name host \\\n",
            "  --description \"Run commands directly in the workspace host environment\" \\\n",
            "  --approval ask\n",
        ),
    )
    .with_context(|| format!("failed to write workspace setup {}", setup_path.display()))?;
    fs::set_permissions(&setup_path, fs::Permissions::from_mode(0o755)).with_context(|| {
        format!(
            "failed to make workspace setup executable {}",
            setup_path.display()
        )
    })?;
    println!("initialized {}", config_path.display());
    Ok(())
}

fn load_workspace_config(workspace: &Path) -> Result<WorkspaceConfig> {
    let path = workspace.join(WORKSPACE_CONFIG);
    let config = fs::read_to_string(&path)
        .with_context(|| format!("failed to read workspace config {}", path.display()))?;
    toml::from_str(&config)
        .with_context(|| format!("failed to parse workspace config {}", path.display()))
}

async fn workspace_start(workspace: &Path, endpoint: &Path, workspace_id: &str) -> Result<()> {
    let config = load_workspace_config(workspace)?;
    let database = controller_database(workspace_id)?;
    controller_start(workspace, endpoint, &database).await?;

    let atra_binary = env::current_exe().context("failed to determine the atra executable path")?;
    let status = TokioCommand::new("bash")
        .args(["-c", &config.setup])
        .current_dir(workspace)
        .env("ATRA_BINARY", &atra_binary)
        .env("ATRA_CONTROLLER_ENDPOINT", endpoint)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("failed to start workspace setup command")?;
    if !status.success() {
        bail!("workspace setup command exited with {status}");
    }
    println!("workspace started");
    Ok(())
}

async fn prepare_tui(workspace: &Path, endpoint: &Path, workspace_id: &str) -> Result<bool> {
    if controller_is_running(endpoint).await? {
        return Ok(true);
    }
    if !workspace.join(WORKSPACE_CONFIG).is_file() {
        bail!("controller is not running");
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("controller is not running");
    }

    print!("Controller is not running. Start this workspace? [y/N] ");
    std::io::stdout()
        .flush()
        .context("failed to display workspace start prompt")?;
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .context("failed to read workspace start confirmation")?;
    if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        return Ok(false);
    }
    workspace_start(workspace, endpoint, workspace_id).await?;
    Ok(true)
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

async fn controller_start(
    workspace: &Path,
    endpoint: &Path,
    database: &Path,
) -> Result<ControllerStart> {
    if controller_is_running(endpoint).await? {
        return Ok(ControllerStart::AlreadyRunning);
    }

    let log_path = database.with_file_name("controller.log");
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)
        .with_context(|| format!("failed to open controller log {}", log_path.display()))?;
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("failed to secure controller log {}", log_path.display()))?;
    let stderr = log
        .try_clone()
        .with_context(|| format!("failed to clone controller log {}", log_path.display()))?;
    let executable = env::current_exe().context("failed to determine the atra executable path")?;
    let mut command = TokioCommand::new(executable);
    command
        .args(["controller", "run"])
        .current_dir(workspace)
        .env("ATRA_CONTROLLER_ENDPOINT", endpoint)
        .env("ATRA_CONTROLLER_STATE", database)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr));
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let mut child = command
        .spawn()
        .context("failed to start background controller")?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if controller_is_running(endpoint).await? {
            return Ok(ControllerStart::Started);
        }
        if let Some(status) = child
            .try_wait()
            .context("failed to inspect background controller")?
        {
            bail!(
                "controller exited with {status}; see {}",
                log_path.display()
            );
        }
        if Instant::now() >= deadline {
            child
                .kill()
                .await
                .context("failed to stop controller after startup timeout")?;
            let _ = child.wait().await;
            bail!(
                "controller did not become ready within 10 seconds; see {}",
                log_path.display()
            );
        }
        sleep(Duration::from_millis(25)).await;
    }
}

async fn controller_stop(endpoint: &Path) -> Result<()> {
    match send_controller_shutdown(endpoint).await {
        Ok(()) => {}
        Err(error) if controller_not_running(&error) => return Ok(()),
        Err(error) => return Err(error),
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    while controller_is_running(endpoint).await? {
        if Instant::now() >= deadline {
            bail!("controller did not stop within 10 seconds");
        }
        sleep(Duration::from_millis(25)).await;
    }
    Ok(())
}

async fn send_controller_shutdown(endpoint: &Path) -> Result<()> {
    let mut stream = UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("failed to connect to controller at {}", endpoint.display()))?;
    let mut request = serde_json::to_vec(&ControllerRequest::Shutdown)
        .context("failed to encode controller shutdown request")?;
    request.push(b'\n');
    stream
        .write_all(&request)
        .await
        .context("failed to write controller shutdown request")?;
    stream
        .shutdown()
        .await
        .context("failed to close controller shutdown request")
}

async fn controller_is_running(endpoint: &Path) -> Result<bool> {
    match send_controller_request(endpoint, ControllerRequest::Status).await {
        Ok(ControllerResponse::Running) => Ok(true),
        Ok(ControllerResponse::Error { message }) => bail!("{message}"),
        Ok(response) => bail!("controller returned an unexpected response: {response:?}"),
        Err(error) if controller_not_running(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn controller_status(endpoint: &Path) -> Result<()> {
    controller_request(endpoint, ControllerRequest::Status).await
}

async fn controller_request(endpoint: &Path, request: ControllerRequest) -> Result<()> {
    let is_status = matches!(request, ControllerRequest::Status);
    let response = send_controller_request(endpoint, request).await;
    let response = match response {
        Ok(response) => response,
        Err(error) if is_status && controller_not_running(&error) => {
            println!("stopped");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    match response {
        ControllerResponse::Running => println!("running"),
        ControllerResponse::Stopping => println!("stopping"),
        ControllerResponse::ThreadRenamed => println!("renamed"),
        ControllerResponse::ThreadModelChanged => println!("model changed"),
        ControllerResponse::ThreadCreated { .. }
        | ControllerResponse::ThreadList { .. }
        | ControllerResponse::ModelList { .. }
        | ControllerResponse::TurnDelta { .. }
        | ControllerResponse::ToolCallStarted { .. }
        | ControllerResponse::ToolCallDelta { .. }
        | ControllerResponse::TurnEvent { .. }
        | ControllerResponse::TurnCompleted { .. }
        | ControllerResponse::ApprovalRequired { .. }
        | ControllerResponse::ThreadEvents { .. }
        | ControllerResponse::RunnerList { .. }
        | ControllerResponse::CodexLoginRequired
        | ControllerResponse::CodexLoggedIn { .. } => {
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
