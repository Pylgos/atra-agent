use std::{
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_client::{Client, ProcessSubscription, ThreadSubscription};
use atra_protocol::{
    ApprovalPolicy, AssistantMessagePhase, CheckpointId, Command as StateCommand, CommandResult,
    EventSequence, HistoryTarget, InteractionId, ProcessId, ProcessLocator, ProcessStatus,
    ProviderLifecycle, ThreadEventData, ThreadId, TurnOutcome,
};
use clap::{Parser, Subcommand, ValueEnum};
use tracing_subscriber::{EnvFilter, fmt::writer::BoxMakeWriter};

mod controller_client;
mod platform;
mod runner;
mod sandbox;
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
    Provider {
        #[command(subcommand)]
        command: ProviderCommand,
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
    Start,
    Clean {
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum ProviderCommand {
    List,
    Login { provider: String },
    Logout { provider: String },
    Status { provider: String },
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
    Install {
        bundle: PathBuf,
    },
    Exec {
        tool: OsString,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
}

#[derive(Subcommand)]
enum RunnerCommand {
    Run {
        #[arg(long)]
        stdio: bool,
    },
    Sandbox(sandbox::SandboxOptions),
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
    if let Command::Runner {
        command: RunnerCommand::Sandbox(options),
    } = &command
    {
        return sandbox::execute(options.clone()).await;
    }
    match &command {
        Command::Platform {
            command: PlatformCommand::Download,
        } => return platform::download().await,
        Command::Platform {
            command: PlatformCommand::Install { bundle },
        } => return platform::install(bundle),
        Command::Platform {
            command: PlatformCommand::Exec { tool, args },
        } => return platform::exec(tool, args).await,
        _ => {}
    }
    let workspace = workspace::root()?;
    let workspace_id = workspace::id(&workspace);
    let endpoint = workspace::endpoint(&workspace_id)?;

    match command {
        Command::Provider {
            command: ProviderCommand::Login { provider },
        } => {
            let auth_home = provider_auth_home()?;
            let credential =
                match atra_controller::provider_auth_method(&auth_home, &provider).await? {
                    atra_protocol::ProviderAuthMethod::None => None,
                    atra_protocol::ProviderAuthMethod::Browser => None,
                    atra_protocol::ProviderAuthMethod::ApiKey => {
                        Some(rpassword::prompt_password(format!("{provider} API key: "))?)
                    }
                };
            match wait_provider_command(
                &endpoint,
                &provider,
                StateCommand::ProviderLogin {
                    provider: provider.clone(),
                    credential: credential.clone(),
                },
            )
            .await
            {
                Ok(ProviderLifecycle::LoggedIn { .. }) => {}
                Ok(status) => bail!("{provider} login ended in state {status:?}"),
                Err(error) if controller_not_running(&error) => {
                    atra_controller::provider_login(&auth_home, &provider, credential).await?;
                }
                Err(error) => return Err(error),
            }
            println!("logged in to {provider}");
            Ok(())
        }
        Command::Provider {
            command: ProviderCommand::Logout { provider },
        } => {
            match wait_provider_command(
                &endpoint,
                &provider,
                StateCommand::ProviderLogout {
                    provider: provider.clone(),
                },
            )
            .await
            {
                Ok(ProviderLifecycle::LoggedOut | ProviderLifecycle::LoginRequired) => {}
                Ok(ProviderLifecycle::LoggedIn { .. }) => {
                    println!(
                        "{provider}: file credential removed; environment credential remains active"
                    );
                    return Ok(());
                }
                Ok(status) => bail!("{provider} logout ended in state {status:?}"),
                Err(error) if controller_not_running(&error) => {
                    let auth_home = provider_auth_home()?;
                    atra_controller::provider_logout(&auth_home, &provider).await?;
                    let (lifecycle, _) =
                        atra_controller::provider_status(&auth_home, &provider).await?;
                    if matches!(lifecycle, ProviderLifecycle::LoggedIn { .. }) {
                        println!(
                            "{provider}: file credential removed; environment credential remains active"
                        );
                        return Ok(());
                    }
                }
                Err(error) => return Err(error),
            }
            println!("logged out of {provider}");
            Ok(())
        }
        Command::Provider {
            command: ProviderCommand::Status { provider },
        } => {
            let (lifecycle, source) = match provider_snapshot(&endpoint, &provider).await {
                Ok(state) => (state.lifecycle().clone(), state.credential_source()),
                Err(error) if controller_not_running(&error) => {
                    atra_controller::provider_status(&provider_auth_home()?, &provider).await?
                }
                Err(error) => return Err(error),
            };
            print_provider_status(&provider, &lifecycle, source);
            Ok(())
        }
        Command::Provider {
            command: ProviderCommand::List,
        } => {
            let providers = match client(&endpoint).subscribe_controller().await {
                Ok(subscription) => subscription.state().providers().to_vec(),
                Err(error) if controller_not_running(&error) => {
                    atra_controller::provider_states(&provider_auth_home()?).await?
                }
                Err(error) => return Err(error),
            };
            for provider in &providers {
                println!(
                    "{}\tauth={:?}\tstatus={:?}\tsource={:?}\tmodels={}",
                    provider.id(),
                    provider.auth_method(),
                    provider.lifecycle(),
                    provider.credential_source(),
                    provider.models().len()
                );
                for model in provider.models() {
                    println!(
                        "  {}\t{}\treasoning=[{}]\ttools=[{}]\tcontext={}",
                        model.id,
                        model.display_name,
                        model.supported_reasoning_efforts.join(","),
                        model
                            .tool_bindings
                            .iter()
                            .map(|binding| format!("{}={}", binding.tool, binding.implementation))
                            .collect::<Vec<_>>()
                            .join(","),
                        model
                            .context_window
                            .map(|value| value.to_string())
                            .unwrap_or_else(|| "unknown".to_owned())
                    );
                }
            }
            Ok(())
        }
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
            command: WorkspaceCommand::Start,
        } => workspace::start(&workspace).await,
        Command::Workspace {
            command: WorkspaceCommand::Clean { force },
        } => workspace::clean(&workspace, &endpoint, force).await,
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
            command: RunnerCommand::Run { .. } | RunnerCommand::Sandbox(_),
        } => unreachable!("runner run and sandbox are handled before workspace setup"),
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
            runner::launch(
                &endpoint,
                runner::RunnerLaunch {
                    name,
                    description,
                    approval: approval.into(),
                    command: runner::runner_command(command)?,
                },
            )
            .await?;
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
            if workspace::prepare_tui(&workspace, &endpoint).await? {
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
                if let atra_protocol::ThreadChange::CheckpointAdded(checkpoint_id) =
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
                        if message.phase == AssistantMessagePhase::FinalAnswer =>
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

fn print_provider_status(
    provider: &str,
    lifecycle: &ProviderLifecycle,
    source: Option<atra_protocol::CredentialSource>,
) {
    match lifecycle {
        ProviderLifecycle::LoggedIn { account } => println!(
            "{provider}: logged in{}{}",
            account
                .as_ref()
                .map(|account| format!(" as {account}"))
                .unwrap_or_default(),
            source
                .map(|source| format!(" ({source:?})"))
                .unwrap_or_default()
        ),
        ProviderLifecycle::LoggedOut | ProviderLifecycle::LoginRequired => {
            println!("{provider}: logged out")
        }
        status => println!("{provider}: {status:?}"),
    }
}

async fn provider_snapshot(
    endpoint: &Path,
    provider_id: &str,
) -> Result<atra_protocol::ProviderState> {
    let subscription = client(endpoint).subscribe_controller().await?;
    subscription
        .state()
        .providers()
        .iter()
        .find(|provider| provider.id() == provider_id)
        .cloned()
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

fn provider_auth_home() -> Result<PathBuf> {
    Ok(data_home()?.join("atra/providers"))
}

fn data_home() -> Result<PathBuf> {
    xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")
}
