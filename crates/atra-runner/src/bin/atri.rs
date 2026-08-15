use std::{
    collections::HashSet,
    env,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_patch::{ApplyPatchResult, PatchOperationOutcome, PatchOperationResult};
use atra_protocol::{
    AgentControlRequestEnvelope, AgentControlResponseEnvelope, AgentRequest, AgentTarget,
    CommandEnvironment, CommandOutput, EventSequence, ProcessHandle, ProcessId, RunnerRequest,
    RunnerRequestEnvelope, RunnerResponse, RunnerResponseEnvelope, ThreadId,
};
use clap::{Parser, Subcommand};
use tokio::{
    io::{self, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    task::JoinHandle,
    time::Instant,
};

const DEFAULT_WAIT_SECONDS: u64 = 120;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Parser)]
#[command(name = "atri")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Proc {
        #[command(subcommand)]
        command: ProcCommand,
    },
    Patch,
    Replace {
        #[arg(long)]
        all: bool,
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum AgentCommand {
    Create {
        #[arg(long)]
        name: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        effort: Option<String>,
    },
    Send {
        thread_id: i64,
        message: Option<String>,
    },
    Wait {
        #[arg(required = true, value_parser = agent_target)]
        targets: Vec<AgentTarget>,
        #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
        timeout: u64,
    },
    List,
    Cancel {
        #[arg(long)]
        recursive: bool,
        #[arg(required = true)]
        thread_ids: Vec<i64>,
    },
    Delete {
        #[arg(long)]
        recursive: bool,
        #[arg(required = true)]
        thread_ids: Vec<i64>,
    },
}

#[derive(Subcommand)]
enum ProcCommand {
    Spawn {
        #[arg(value_parser = process_id)]
        process: String,
        #[arg(allow_hyphen_values = true)]
        command: String,
    },
    Wait {
        #[arg(required = true, value_parser = process_id)]
        processes: Vec<String>,
        #[arg(long, default_value_t = DEFAULT_WAIT_SECONDS)]
        timeout: u64,
    },
    Stop {
        #[arg(required = true, value_parser = process_id)]
        processes: Vec<String>,
    },
}

enum ProcessState {
    Running,
    Exited(Option<i32>),
    Stopped,
    Error(String),
}

struct ProcessResult {
    process: String,
    state: ProcessState,
    output: CollectedOutput,
}

#[derive(Default)]
struct CollectedOutput {
    bytes: Vec<u8>,
    omitted_bytes: usize,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(success) => ExitCode::from(u8::from(!success)),
        Err(error) => {
            eprintln!("atri: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<bool> {
    let endpoint = PathBuf::from(
        env::var_os("ATRI_RUNNER_ENDPOINT")
            .context("ATRI_RUNNER_ENDPOINT is not set; atri must run on an Atra Runner")?,
    );
    match cli.command {
        Command::Agent { command } => run_agent(&endpoint, command).await,
        Command::Proc { command } => run_proc(endpoint, command).await,
        Command::Patch => run_patch(&endpoint).await,
        Command::Replace { all, path } => run_replace(&endpoint, path, all).await,
    }
}

async fn run_agent(endpoint: &Path, command: AgentCommand) -> Result<bool> {
    let request = match command {
        AgentCommand::Create {
            name,
            model,
            effort,
        } => AgentRequest::Create {
            name,
            model,
            effort,
        },
        AgentCommand::Send { thread_id, message } => {
            let message = match message {
                Some(message) => message,
                None => {
                    if std::io::stdin().is_terminal() {
                        bail!("message is required when stdin is a TTY");
                    }
                    let mut message = String::new();
                    io::stdin()
                        .read_to_string(&mut message)
                        .await
                        .context("failed to read agent message from stdin")?;
                    message
                }
            };
            AgentRequest::Send {
                thread_id: ThreadId(thread_id),
                message,
            }
        }
        AgentCommand::Wait { targets, timeout } => AgentRequest::Wait {
            targets,
            timeout_ms: timeout.checked_mul(1000).context("timeout is too large")?,
        },
        AgentCommand::List => AgentRequest::List,
        AgentCommand::Cancel {
            recursive,
            thread_ids,
        } => AgentRequest::Cancel {
            thread_ids: thread_ids.into_iter().map(ThreadId).collect(),
            recursive,
        },
        AgentCommand::Delete {
            recursive,
            thread_ids,
        } => AgentRequest::Delete {
            thread_ids: thread_ids.into_iter().map(ThreadId).collect(),
            recursive,
        },
    };
    let process_handle = ProcessHandle(
        env::var("ATRI_PROCESS_HANDLE")
            .context("ATRI_PROCESS_HANDLE is not set; atri must run as an Atra command")?,
    );
    let response = request_agent(endpoint, process_handle, request).await?;
    if !response.output.is_empty() {
        println!("{}", response.output);
    }
    Ok(response.success)
}

fn agent_target(value: &str) -> std::result::Result<AgentTarget, String> {
    let (thread, after_sequence) = match value.rsplit_once('@') {
        Some((thread, sequence)) => {
            let sequence = sequence
                .parse::<i64>()
                .map_err(|_| "sequence must be -1 or a non-negative integer".to_owned())?;
            if sequence < -1 {
                return Err("sequence must be -1 or a non-negative integer".to_owned());
            }
            (thread, Some(EventSequence(sequence)))
        }
        None => (value, None),
    };
    let thread_id = thread
        .parse::<i64>()
        .map_err(|_| "thread ID must be an integer".to_owned())?;
    Ok(AgentTarget {
        thread_id: ThreadId(thread_id),
        after_sequence,
    })
}

async fn request_agent(
    endpoint: &Path,
    process_handle: ProcessHandle,
    request: AgentRequest,
) -> Result<atra_protocol::AgentResponse> {
    let mut stream = UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("failed to connect to Runner at {}", endpoint.display()))?;
    let mut encoded = serde_json::to_vec(&AgentControlRequestEnvelope {
        request_id: 0,
        process_handle,
        request,
    })
    .context("failed to encode agent request")?;
    encoded.push(b'\n');
    stream
        .write_all(&encoded)
        .await
        .context("failed to write agent request")?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .await
        .context("failed to read agent response")?;
    Ok(serde_json::from_str::<AgentControlResponseEnvelope>(&line)
        .context("failed to decode agent response")?
        .response)
}

async fn run_replace(endpoint: &Path, path: PathBuf, replace_all: bool) -> Result<bool> {
    let process_handle = ProcessHandle(
        env::var("ATRI_PROCESS_HANDLE")
            .context("ATRI_PROCESS_HANDLE is not set; atri must run as an Atra command")?,
    );
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .await
        .context("failed to read replacement from stdin")?;
    let input = input
        .strip_prefix("--- Old\n")
        .context("replacement must start with '--- Old'")?;
    let (old, new) = input
        .split_once("\n--- New\n")
        .context("replacement must contain a line with '--- New'")?;
    let new = new.strip_suffix('\n').unwrap_or(new);
    match request(
        endpoint,
        RunnerRequest::ReplaceText {
            process_handle,
            cwd,
            path,
            old: old.to_owned(),
            new: new.to_owned(),
            replace_all,
        },
    )
    .await?
    {
        RunnerResponse::ReplaceCompleted { result } => Ok(display_patch_result(&result)),
        RunnerResponse::Error { message } => bail!("{message}"),
        _ => bail!("Runner returned an invalid replace response"),
    }
}

async fn run_proc(endpoint: PathBuf, command: ProcCommand) -> Result<bool> {
    if let ProcCommand::Spawn { process, command } = command {
        let parent_process_handle = ProcessHandle(
            env::var("ATRI_PROCESS_HANDLE")
                .context("ATRI_PROCESS_HANDLE is not set; atri must run as an Atra command")?,
        );
        let cwd = env::current_dir().context("failed to determine the current directory")?;
        let environment = CommandEnvironment {
            set: env::vars().collect(),
            ..CommandEnvironment::default()
        };
        return match request(
            &endpoint,
            RunnerRequest::SpawnProcess {
                parent_process_handle,
                command,
                cwd,
                environment,
                process_id: ProcessId(process.clone()),
            },
        )
        .await?
        {
            RunnerResponse::ProcessStarted { .. } => {
                println!("Process started with ID {process}");
                Ok(true)
            }
            RunnerResponse::Error { message } => bail!("{message}"),
            _ => bail!("Runner returned an invalid spawn response"),
        };
    }

    let prefix = env::var("ATRI_PROCESS_PREFIX")
        .context("ATRI_PROCESS_PREFIX is not set; atri must run on an Atra Runner")?;
    let waiting_process_handle = ProcessHandle(
        env::var("ATRI_PROCESS_HANDLE")
            .context("ATRI_PROCESS_HANDLE is not set; atri must run as an Atra command")?,
    );
    let processes = match &command {
        ProcCommand::Wait { processes, .. } | ProcCommand::Stop { processes } => processes,
        ProcCommand::Spawn { .. } => unreachable!(),
    };
    let mut unique = HashSet::new();
    if let Some(process) = processes
        .iter()
        .find(|process| !unique.insert(process.as_str()))
    {
        bail!("process '{process}' was specified more than once");
    }

    let results = match command {
        ProcCommand::Wait { processes, timeout } => {
            let deadline = Instant::now()
                .checked_add(Duration::from_secs(timeout))
                .context("timeout is too large")?;
            let tasks = processes
                .into_iter()
                .map(|process| {
                    let endpoint = endpoint.clone();
                    let handle = ProcessHandle(format!("{prefix}{process}"));
                    let waiting_process_handle = waiting_process_handle.clone();
                    tokio::spawn(wait_process(
                        endpoint,
                        process,
                        waiting_process_handle,
                        handle,
                        deadline,
                    ))
                })
                .collect::<Vec<_>>();
            collect_results(tasks).await
        }
        ProcCommand::Stop { processes } => {
            let tasks = processes
                .into_iter()
                .map(|process| {
                    let endpoint = endpoint.clone();
                    let handle = ProcessHandle(format!("{prefix}{process}"));
                    tokio::spawn(stop_process(endpoint, process, handle))
                })
                .collect::<Vec<_>>();
            collect_results(tasks).await
        }
        ProcCommand::Spawn { .. } => unreachable!(),
    };

    let success = results
        .iter()
        .all(|result| !matches!(result.state, ProcessState::Error(_)));
    display_results(results);
    Ok(success)
}

async fn run_patch(endpoint: &Path) -> Result<bool> {
    let process_handle = ProcessHandle(
        env::var("ATRI_PROCESS_HANDLE")
            .context("ATRI_PROCESS_HANDLE is not set; atri must run as an Atra command")?,
    );
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let mut patch = String::new();
    io::stdin()
        .read_to_string(&mut patch)
        .await
        .context("failed to read patch from stdin")?;
    let patch = patch
        .strip_prefix("*** Begin Patch\n")
        .context("patch must start with '*** Begin Patch'")?;
    let patch = patch
        .strip_suffix("*** End Patch\n")
        .or_else(|| patch.strip_suffix("*** End Patch"))
        .context("patch must end with '*** End Patch'")?
        .to_owned();
    match request(
        endpoint,
        RunnerRequest::ApplyPatch {
            process_handle,
            cwd,
            patch,
        },
    )
    .await?
    {
        RunnerResponse::PatchCompleted { result } => Ok(display_patch_result(&result)),
        RunnerResponse::Error { message } => bail!("{message}"),
        _ => bail!("Runner returned an invalid patch response"),
    }
}

fn display_patch_result(result: &ApplyPatchResult) -> bool {
    let results = match result {
        ApplyPatchResult::ParseError { error } => {
            println!("atri patch failed:\n{error}");
            return false;
        }
        ApplyPatchResult::Operations { results } => results,
    };
    let success = results.iter().all(|result| {
        matches!(
            result,
            PatchOperationResult::Added {
                outcome: PatchOperationOutcome::Applied { .. },
                ..
            } | PatchOperationResult::Deleted {
                outcome: PatchOperationOutcome::Applied { .. },
                ..
            } | PatchOperationResult::Updated {
                outcome: PatchOperationOutcome::Applied { .. },
                ..
            } | PatchOperationResult::Moved {
                outcome: PatchOperationOutcome::Applied { .. },
                ..
            }
        )
    });
    if success {
        println!("Success. Updated the following files:");
    } else {
        println!("atri patch completed with errors:");
    }
    for result in results {
        let (label, outcome) = match result {
            PatchOperationResult::Added { path, outcome } => {
                (format!("A {}", path.display()), outcome)
            }
            PatchOperationResult::Deleted { path, outcome } => {
                (format!("D {}", path.display()), outcome)
            }
            PatchOperationResult::Updated { path, outcome } => {
                (format!("M {}", path.display()), outcome)
            }
            PatchOperationResult::Moved { from, to, outcome } => {
                (format!("R {} -> {}", from.display(), to.display()), outcome)
            }
        };
        match outcome {
            PatchOperationOutcome::Applied { .. } => println!("{label}"),
            PatchOperationOutcome::Failed { error } => println!("{label}: {error}"),
        }
    }
    success
}

async fn collect_results(tasks: Vec<JoinHandle<ProcessResult>>) -> Vec<ProcessResult> {
    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        results.push(task.await.expect("atri process task panicked"));
    }
    results
}

async fn wait_process(
    endpoint: PathBuf,
    process: String,
    waiting_process_handle: ProcessHandle,
    handle: ProcessHandle,
    deadline: Instant,
) -> ProcessResult {
    let mut output = CollectedOutput::default();
    loop {
        let timeout_ms = deadline
            .saturating_duration_since(Instant::now())
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        match request(
            &endpoint,
            RunnerRequest::WaitChildProcess {
                waiting_process_handle: waiting_process_handle.clone(),
                process_handle: handle.clone(),
                timeout_ms,
            },
        )
        .await
        {
            Ok(RunnerResponse::ProcessRunning {
                output: command_output,
                ..
            }) => {
                output.append(command_output);
                if Instant::now() >= deadline {
                    return ProcessResult {
                        process,
                        state: ProcessState::Running,
                        output,
                    };
                }
            }
            Ok(RunnerResponse::ProcessFinished {
                output: command_output,
                exit_code,
                ..
            }) => {
                output.append(command_output);
                return ProcessResult {
                    process,
                    state: ProcessState::Exited(exit_code),
                    output,
                };
            }
            Ok(RunnerResponse::Error { message }) => {
                return ProcessResult {
                    process,
                    state: ProcessState::Error(message),
                    output,
                };
            }
            Ok(_) => {
                return ProcessResult {
                    process,
                    state: ProcessState::Error(
                        "Runner returned an invalid wait response".to_owned(),
                    ),
                    output,
                };
            }
            Err(error) => {
                return ProcessResult {
                    process,
                    state: ProcessState::Error(format!("{error:#}")),
                    output,
                };
            }
        }
    }
}

async fn stop_process(endpoint: PathBuf, process: String, handle: ProcessHandle) -> ProcessResult {
    match request(
        &endpoint,
        RunnerRequest::StopProcess {
            process_handle: handle,
        },
    )
    .await
    {
        Ok(RunnerResponse::ProcessStopped { output }) => {
            let mut collected = CollectedOutput::default();
            collected.append(output);
            ProcessResult {
                process,
                state: ProcessState::Stopped,
                output: collected,
            }
        }
        Ok(RunnerResponse::Error { message }) => ProcessResult {
            process,
            state: ProcessState::Error(message),
            output: CollectedOutput::default(),
        },
        Ok(_) => ProcessResult {
            process,
            state: ProcessState::Error("Runner returned an invalid stop response".to_owned()),
            output: CollectedOutput::default(),
        },
        Err(error) => ProcessResult {
            process,
            state: ProcessState::Error(format!("{error:#}")),
            output: CollectedOutput::default(),
        },
    }
}

async fn request(endpoint: &Path, request: RunnerRequest) -> Result<RunnerResponse> {
    let mut stream = UnixStream::connect(endpoint)
        .await
        .with_context(|| format!("failed to connect to Runner at {}", endpoint.display()))?;
    let mut message = serde_json::to_vec(&RunnerRequestEnvelope {
        request_id: 0,
        request,
    })
    .context("failed to encode Runner request")?;
    message.push(b'\n');
    stream
        .write_all(&message)
        .await
        .context("failed to send Runner request")?;

    let mut line = String::new();
    if BufReader::new(stream)
        .read_line(&mut line)
        .await
        .context("failed to read Runner response")?
        == 0
    {
        bail!("Runner disconnected before sending a response");
    }
    let envelope: RunnerResponseEnvelope =
        serde_json::from_str(&line).context("failed to decode Runner response")?;
    Ok(envelope.response)
}

fn display_results(results: Vec<ProcessResult>) {
    for (index, result) in results.into_iter().enumerate() {
        if index != 0 {
            println!();
        }
        let status = match &result.state {
            ProcessState::Running => "running".to_owned(),
            ProcessState::Exited(Some(exit_code)) => format!("exited {exit_code}"),
            ProcessState::Exited(None) => "exited without code".to_owned(),
            ProcessState::Stopped => "stopped".to_owned(),
            ProcessState::Error(_) => "error".to_owned(),
        };
        let omitted = if result.output.omitted_bytes != 0 {
            format!(", {} bytes omitted", result.output.omitted_bytes)
        } else {
            Default::default()
        };
        println!("==> {} [{status}{omitted}] <==", result.process);
        let content = String::from_utf8_lossy(&result.output.bytes);
        if !content.is_empty() {
            print!("{content}");
            if !content.ends_with('\n') {
                println!();
            }
        }
        if let ProcessState::Error(message) = result.state {
            println!("{message}");
        }
    }
}

impl CollectedOutput {
    fn append(&mut self, output: CommandOutput) {
        self.omitted_bytes += output.omitted_bytes;
        let bytes = output.content.as_bytes();
        let total_len = self.bytes.len() + bytes.len();
        if total_len <= MAX_OUTPUT_BYTES {
            self.bytes.extend_from_slice(bytes);
            return;
        }

        let head_len = MAX_OUTPUT_BYTES / 2;
        let tail_len = MAX_OUTPUT_BYTES - head_len;
        let old = std::mem::take(&mut self.bytes);
        self.bytes = Vec::with_capacity(MAX_OUTPUT_BYTES);

        let old_head_len = head_len.min(old.len());
        self.bytes.extend_from_slice(&old[..old_head_len]);
        self.bytes
            .extend_from_slice(&bytes[..head_len - old_head_len]);

        if bytes.len() >= tail_len {
            self.bytes
                .extend_from_slice(&bytes[bytes.len() - tail_len..]);
        } else {
            self.bytes
                .extend_from_slice(&old[old.len() - (tail_len - bytes.len())..]);
            self.bytes.extend_from_slice(bytes);
        }
        self.omitted_bytes += total_len - MAX_OUTPUT_BYTES;
    }
}

fn process_id(value: &str) -> Result<String, String> {
    let valid = value.len() <= 64
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        });
    if valid {
        Ok(value.to_owned())
    } else {
        Err("must match [a-z][a-z0-9_-]{0,63}".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_defaults_to_120_seconds() {
        let cli = Cli::try_parse_from(["atri", "proc", "wait", "process"]).unwrap();
        let Command::Proc {
            command: ProcCommand::Wait { timeout, .. },
        } = cli.command
        else {
            panic!("expected proc wait");
        };

        assert_eq!(timeout, 120);
    }

    #[test]
    fn wait_timeout_has_no_configured_maximum() {
        let cli =
            Cli::try_parse_from(["atri", "proc", "wait", "process", "--timeout", "86400"]).unwrap();
        let Command::Proc {
            command: ProcCommand::Wait { timeout, .. },
        } = cli.command
        else {
            panic!("expected proc wait");
        };

        assert_eq!(timeout, 86_400);
    }

    #[test]
    fn agent_wait_parses_sentinel_cursor_and_unbounded_timeout() {
        let cli =
            Cli::try_parse_from(["atri", "agent", "wait", "42@-1", "--timeout", "86400"]).unwrap();
        let Command::Agent {
            command: AgentCommand::Wait { targets, timeout },
        } = cli.command
        else {
            panic!("expected agent wait")
        };
        assert_eq!(targets[0].thread_id, ThreadId(42));
        assert_eq!(targets[0].after_sequence, Some(EventSequence(-1)));
        assert_eq!(timeout, 86_400);
    }

    #[test]
    fn agent_create_requires_a_name() {
        assert!(Cli::try_parse_from(["atri", "agent", "create"]).is_err());
    }
}
