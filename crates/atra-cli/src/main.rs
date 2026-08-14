use std::{
    env,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_client::{Client, ProcessSubscription, ThreadSubscription};
use atra_protocol::{
    ApprovalPolicy, AssistantMessagePhase, CheckpointId, Command as StateCommand, CommandResult,
    EventSequence, HistoryTarget, InteractionId, ProcessId, ProcessLocator, ProcessStatus,
    ProviderLifecycle, RunnerLifecycle, ThreadEventData, ThreadId, TurnOutcome,
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
    Ollama {
        #[command(subcommand)]
        command: OllamaCommand,
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
enum OllamaCommand {
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
        provider: String,
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
    Compact {
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
            atra_controller::codex_login(&provider_auth_home()?.join("codex")).await?;
            match wait_provider_command(
                &endpoint,
                "codex",
                StateCommand::ProviderReloadAuth {
                    provider: "codex".to_owned(),
                },
            )
            .await
            {
                Ok(ProviderLifecycle::LoggedIn { .. }) => {}
                Ok(status) => bail!("Codex login ended in state {status:?}"),
                Err(error) if controller_not_running(&error) => {}
                Err(error) => return Err(error),
            }
            println!("logged in");
            Ok(())
        }
        Command::Codex {
            command: CodexCommand::Logout,
        } => {
            match wait_provider_command(
                &endpoint,
                "codex",
                StateCommand::ProviderLogout {
                    provider: "codex".to_owned(),
                },
            )
            .await
            {
                Ok(ProviderLifecycle::LoggedOut | ProviderLifecycle::LoginRequired) => {}
                Ok(status) => bail!("Codex logout ended in state {status:?}"),
                Err(error) if controller_not_running(&error) => {
                    atra_controller::codex_logout(&provider_auth_home()?.join("codex")).await?;
                }
                Err(error) => return Err(error),
            }
            println!("logged out");
            Ok(())
        }
        Command::Codex {
            command: CodexCommand::Status,
        } => match provider_lifecycle(&endpoint, "codex").await? {
            ProviderLifecycle::LoggedIn { account } => {
                println!(
                    "logged in{}",
                    account
                        .map(|account| format!(" as {account}"))
                        .unwrap_or_default()
                );
                Ok(())
            }
            ProviderLifecycle::LoggedOut | ProviderLifecycle::LoginRequired => {
                println!("logged out");
                Ok(())
            }
            status => bail!("Codex provider is in state {status:?}"),
        },
        Command::Ollama {
            command: OllamaCommand::Login,
        } => {
            let api_key = rpassword::prompt_password("Ollama API key: ")?;
            match wait_provider_command(
                &endpoint,
                "ollama",
                StateCommand::ProviderLogin {
                    provider: "ollama".to_owned(),
                    credential: Some(api_key.clone()),
                },
            )
            .await
            {
                Ok(ProviderLifecycle::LoggedIn { .. }) => {}
                Ok(status) => bail!("Ollama login ended in state {status:?}"),
                Err(error) if controller_not_running(&error) => {
                    atra_controller::ollama_login(&provider_auth_home()?.join("ollama"), api_key)
                        .await?;
                }
                Err(error) => return Err(error),
            }
            println!("logged in");
            Ok(())
        }
        Command::Ollama {
            command: OllamaCommand::Logout,
        } => {
            match wait_provider_command(
                &endpoint,
                "ollama",
                StateCommand::ProviderLogout {
                    provider: "ollama".to_owned(),
                },
            )
            .await
            {
                Ok(ProviderLifecycle::LoggedOut | ProviderLifecycle::LoginRequired) => {}
                Ok(status) => bail!("Ollama logout ended in state {status:?}"),
                Err(error) if controller_not_running(&error) => {
                    atra_controller::ollama_logout(&provider_auth_home()?.join("ollama")).await?;
                }
                Err(error) => return Err(error),
            }
            println!("logged out");
            Ok(())
        }
        Command::Ollama {
            command: OllamaCommand::Status,
        } => match provider_lifecycle(&endpoint, "ollama").await? {
            ProviderLifecycle::LoggedIn { .. } => {
                println!("logged in");
                Ok(())
            }
            ProviderLifecycle::LoggedOut | ProviderLifecycle::LoginRequired => {
                println!("logged out");
                Ok(())
            }
            status => bail!("Ollama provider is in state {status:?}"),
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
            let data_home = data_home()?;
            atra_controller::run(
                &endpoint,
                &database,
                &data_home.join("atra/providers"),
                &data_home,
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
        } => {
            expect_accepted(
                client(&endpoint)
                    .command(StateCommand::ApprovalAllow {
                        approval_id: InteractionId(approval),
                    })
                    .await?,
            )?;
            Ok(())
        }
        Command::Approval {
            command: ApprovalCommand::Deny { approval, reason },
        } => {
            expect_accepted(
                client(&endpoint)
                    .command(StateCommand::ApprovalDeny {
                        approval_id: InteractionId(approval),
                        reason,
                    })
                    .await?,
            )?;
            Ok(())
        }
        Command::Runner {
            command: RunnerCommand::Run { .. },
        } => unreachable!("runner run is handled before workspace setup"),
        Command::Runner {
            command: RunnerCommand::List,
        } => {
            let subscription = client(&endpoint).subscribe_controller().await?;
            for runner in subscription.state().runners() {
                println!(
                    "{}\t{}\t{:?}",
                    runner.runner().name,
                    runner.runner().description,
                    runner.lifecycle()
                );
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
            let mut subscription = client(&endpoint).subscribe_controller().await?;
            expect_accepted(
                client(&endpoint)
                    .command(StateCommand::RunnerLaunch {
                        name: name.clone(),
                        description,
                        approval: approval.into(),
                        command,
                    })
                    .await?,
            )?;
            loop {
                subscription.receive().await?;
                let lifecycle = subscription
                    .state()
                    .runners()
                    .iter()
                    .find(|runner| runner.runner().name == name)
                    .with_context(|| format!("Runner {name} is not available"))?
                    .lifecycle();
                match lifecycle {
                    RunnerLifecycle::Launching => {}
                    RunnerLifecycle::Running => break,
                    RunnerLifecycle::Failed { message } => bail!(message.clone()),
                }
            }
            println!("launched");
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
            if background && timeout_ms.is_some() {
                bail!("background commands cannot have timeout options");
            }
            let process_id = match client(&endpoint)
                .command(StateCommand::ExecCommand {
                    thread_id: ThreadId(thread),
                    runner: name.clone(),
                    command,
                })
                .await?
            {
                CommandResult::ProcessStarted { process_id } => process_id,
                result => bail!("unexpected command result: {result:?}"),
            };
            if background {
                println!("{process_id}");
                Ok(())
            } else {
                let subscription = client(&endpoint)
                    .subscribe_process(ProcessLocator::new(
                        ThreadId(thread),
                        name,
                        process_id.clone(),
                    ))
                    .await?;
                display_process_until_terminal(subscription, timeout_ms, Some(process_id)).await
            }
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
        } => {
            let process_id = ProcessId(process_id);
            let subscription = client(&endpoint)
                .subscribe_process(ProcessLocator::new(
                    ThreadId(thread),
                    name,
                    process_id.clone(),
                ))
                .await?;
            display_process_until_terminal(subscription, Some(timeout_ms), Some(process_id)).await
        }
        Command::Runner {
            command:
                RunnerCommand::Stop {
                    thread,
                    name,
                    process_id,
                },
        } => {
            let locator = ProcessLocator::new(ThreadId(thread), name, ProcessId(process_id));
            let subscription = client(&endpoint).subscribe_process(locator.clone()).await?;
            expect_accepted(
                client(&endpoint)
                    .command(StateCommand::StopProcess { process: locator })
                    .await?,
            )?;
            let mut subscription = subscription;
            wait_for_process(&mut subscription).await?;
            print!("{}", subscription.state().output_tail());
            Ok(())
        }
    }
}

async fn run_thread(endpoint: &Path, command: ThreadCommand) -> Result<()> {
    match command {
        ThreadCommand::Create { name } => {
            match client(endpoint)
                .command(StateCommand::ThreadCreate { display_name: name })
                .await?
            {
                CommandResult::ThreadCreated { thread_id } => println!("{thread_id}"),
                result => bail!("unexpected command result: {result:?}"),
            }
            Ok(())
        }
        ThreadCommand::List => {
            let subscription = client(endpoint).subscribe_controller().await?;
            for thread in subscription.state().threads() {
                println!(
                    "{}\t{}",
                    thread.id,
                    thread.display_name.as_deref().unwrap_or("")
                );
            }
            Ok(())
        }
        ThreadCommand::Rename { thread, name } => {
            expect_accepted(
                client(endpoint)
                    .command(StateCommand::ThreadRename {
                        thread_id: ThreadId(thread),
                        display_name: name,
                    })
                    .await?,
            )?;
            println!("renamed");
            Ok(())
        }
        ThreadCommand::Model {
            thread,
            provider,
            model,
            reasoning_effort,
        } => {
            expect_accepted(
                client(endpoint)
                    .command(StateCommand::ThreadSetModel {
                        thread_id: ThreadId(thread),
                        provider,
                        model,
                        reasoning_effort,
                    })
                    .await?,
            )?;
            println!("model changed");
            Ok(())
        }
        ThreadCommand::Send { thread, message } => {
            let thread_id = ThreadId(thread);
            let subscription = client(endpoint).subscribe_thread(thread_id).await?;
            display_turn(
                client(endpoint),
                subscription,
                StateCommand::ThreadSend {
                    thread_id,
                    message,
                    allow_questions: false,
                },
                false,
            )
            .await
        }
        ThreadCommand::Events { thread } => {
            let subscription = client(endpoint).subscribe_thread(ThreadId(thread)).await?;
            for event in subscription.state().events() {
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
                match client(endpoint)
                    .command(StateCommand::ThreadFork {
                        thread_id: ThreadId(thread),
                        checkpoint_id: checkpoint.map(CheckpointId),
                        sequence: EventSequence(sequence),
                        display_name: name,
                    })
                    .await?
                {
                    CommandResult::ThreadForked { thread_id } => thread_id,
                    result => bail!("unexpected command result: {result:?}"),
                }
            );
            Ok(())
        }
        ThreadCommand::Rewind {
            thread,
            checkpoint,
            sequence,
        } => {
            expect_accepted(
                client(endpoint)
                    .command(StateCommand::ThreadReplaceHistory {
                        thread_id: ThreadId(thread),
                        target: HistoryTarget::Message {
                            checkpoint_id: checkpoint.map(CheckpointId),
                            sequence: EventSequence(sequence),
                        },
                    })
                    .await?,
            )?;
            println!("rewound");
            Ok(())
        }
        ThreadCommand::Continue { thread } => {
            let thread_id = ThreadId(thread);
            let subscription = client(endpoint).subscribe_thread(thread_id).await?;
            display_turn(
                client(endpoint),
                subscription,
                StateCommand::ThreadContinue {
                    thread_id,
                    allow_questions: false,
                },
                false,
            )
            .await
        }
        ThreadCommand::Compact { thread } => {
            let thread_id = ThreadId(thread);
            let subscription = client(endpoint).subscribe_thread(thread_id).await?;
            display_turn(
                client(endpoint),
                subscription,
                StateCommand::ThreadCompact {
                    thread_id,
                    allow_questions: false,
                },
                true,
            )
            .await
        }
    }
}

async fn run_checkpoint(endpoint: &Path, command: CheckpointCommand) -> Result<()> {
    match command {
        CheckpointCommand::Create { thread } => {
            let thread_id = ThreadId(thread);
            let mut subscription = client(endpoint).subscribe_thread(thread_id).await?;
            expect_accepted(
                client(endpoint)
                    .command(StateCommand::ThreadCheckpointCreate { thread_id })
                    .await?,
            )?;
            loop {
                if let atra_protocol::ThreadChange::Checkpoint(checkpoint_id) =
                    subscription.receive().await?
                {
                    println!("{checkpoint_id}");
                    break;
                }
            }
            Ok(())
        }
        CheckpointCommand::List { thread } => {
            let subscription = client(endpoint).subscribe_thread(ThreadId(thread)).await?;
            for checkpoint in subscription.state().checkpoints() {
                println!(
                    "{}\t{}\t{}",
                    checkpoint.id, checkpoint.created_at_ms, checkpoint.reason
                );
            }
            Ok(())
        }
        CheckpointCommand::Events { checkpoint } => {
            let subscription = client(endpoint)
                .subscribe_checkpoint(CheckpointId(checkpoint))
                .await?;
            for event in subscription.state().events() {
                println!(
                    "{}",
                    serde_json::to_string(&event).context("failed to encode checkpoint event")?
                );
            }
            Ok(())
        }
        CheckpointCommand::Restore { thread, checkpoint } => {
            expect_accepted(
                client(endpoint)
                    .command(StateCommand::ThreadReplaceHistory {
                        thread_id: ThreadId(thread),
                        target: HistoryTarget::Checkpoint {
                            checkpoint_id: CheckpointId(checkpoint),
                        },
                    })
                    .await?,
            )?;
            println!("restored");
            Ok(())
        }
    }
}

async fn display_turn(
    client: Client,
    mut subscription: ThreadSubscription,
    command: StateCommand,
    compacting: bool,
) -> Result<()> {
    expect_accepted(client.command(command).await?)?;
    let mut displayed_approval = None;
    loop {
        subscription.receive().await?;
        if let Some(approval) = subscription
            .state()
            .active_turn()
            .and_then(|turn| turn.pending_approval())
            && displayed_approval != Some(approval.id())
        {
            displayed_approval = Some(approval.id());
            println!("{}", approval.id());
            println!("tool: {}", approval.tool());
            println!(
                "arguments: {}",
                serde_json::to_string(approval.arguments())
                    .context("failed to encode tool arguments")?
            );
            std::io::stdout()
                .flush()
                .context("failed to flush approval request")?;
        }
        if let Some(outcome) = subscription.state().last_outcome() {
            return display_turn_outcome(subscription.state(), outcome, compacting);
        }
    }
}

fn display_turn_outcome(
    state: &atra_protocol::ThreadState,
    outcome: &TurnOutcome,
    compacting: bool,
) -> Result<()> {
    match outcome {
        TurnOutcome::Cancelled => {
            println!("cancelled");
            Ok(())
        }
        TurnOutcome::Completed if compacting => {
            println!("compacted");
            Ok(())
        }
        TurnOutcome::Completed => {
            if let Some(content) = state
                .events()
                .iter()
                .rev()
                .find_map(|event| match &event.data {
                    ThreadEventData::AssistantMessage(message)
                        if message.phase != Some(AssistantMessagePhase::Commentary) =>
                    {
                        Some(message.content.as_str())
                    }
                    _ => None,
                })
            {
                println!("{content}");
            }
            Ok(())
        }
        TurnOutcome::Failed { message } => bail!(message.clone()),
    }
}

async fn display_process_until_terminal(
    mut subscription: ProcessSubscription,
    timeout_ms: Option<u64>,
    process_id: Option<ProcessId>,
) -> Result<()> {
    let timed_out = match timeout_ms {
        Some(timeout_ms) => match tokio::time::timeout(
            std::time::Duration::from_millis(timeout_ms),
            wait_for_process(&mut subscription),
        )
        .await
        {
            Ok(result) => {
                result?;
                false
            }
            Err(_) => true,
        },
        None => {
            wait_for_process(&mut subscription).await?;
            false
        }
    };
    if timed_out {
        print!("{}", subscription.state().output_tail());
        if let Some(process_id) = process_id {
            eprintln!("process \"{process_id}\" is still running");
        }
        return Ok(());
    }
    print!("{}", subscription.state().output_tail());
    match subscription.state().process().status() {
        ProcessStatus::Running => Ok(()),
        ProcessStatus::Exited { exit_code } => {
            if *exit_code != Some(0) {
                bail!(
                    "command exited with {}",
                    exit_code
                        .map(|code| format!("status {code}"))
                        .unwrap_or_else(|| "a signal".to_owned())
                );
            }
            Ok(())
        }
        ProcessStatus::Unavailable { message } => bail!(message.clone()),
    }
}

async fn wait_for_process(subscription: &mut ProcessSubscription) -> Result<()> {
    while matches!(
        subscription.state().process().status(),
        ProcessStatus::Running
    ) {
        subscription.receive().await?;
    }
    Ok(())
}

fn expect_accepted(result: CommandResult) -> Result<()> {
    match result {
        CommandResult::Accepted => Ok(()),
        result => bail!("unexpected command result: {result:?}"),
    }
}

async fn provider_lifecycle(endpoint: &Path, provider_id: &str) -> Result<ProviderLifecycle> {
    let subscription = client(endpoint).subscribe_controller().await?;
    subscription
        .state()
        .providers()
        .iter()
        .find(|provider| provider.id() == provider_id)
        .map(|provider| provider.lifecycle().clone())
        .with_context(|| format!("provider {provider_id} is not available"))
}

async fn wait_provider_command(
    endpoint: &Path,
    provider_id: &str,
    command: StateCommand,
) -> Result<ProviderLifecycle> {
    let mut subscription = client(endpoint).subscribe_controller().await?;
    expect_accepted(client(endpoint).command(command).await?)?;
    loop {
        subscription.receive().await?;
        let lifecycle = subscription
            .state()
            .providers()
            .iter()
            .find(|provider| provider.id() == provider_id)
            .with_context(|| format!("provider {provider_id} is not available"))?
            .lifecycle();
        if !matches!(
            lifecycle,
            ProviderLifecycle::LoggingIn
                | ProviderLifecycle::LoggingOut
                | ProviderLifecycle::Refreshing
        ) {
            return Ok(lifecycle.clone());
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

fn provider_auth_home() -> Result<PathBuf> {
    Ok(data_home()?.join("atra/providers"))
}

fn data_home() -> Result<PathBuf> {
    xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")
}
