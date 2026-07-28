use super::*;

#[derive(Deserialize, serde::Serialize)]
pub(super) struct ExecCommandArguments {
    pub(super) runner: String,
    pub(super) command: String,
    #[serde(flatten)]
    pub(super) mode: ModelCommandMode,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum ModelCommandMode {
    Foreground { timeout_ms: u64 },
    Background { process_id: ProcessId },
    Timed { timeout_ms: u64 },
}

#[derive(Deserialize, serde::Serialize)]
pub(super) struct ApplyPatchArguments {
    pub(super) runner: String,
    pub(super) patch: String,
}

#[derive(Deserialize, serde::Serialize)]
pub(super) struct WaitProcessArguments {
    pub(super) runner: String,
    pub(super) process_id: ProcessId,
    pub(super) timeout_ms: u64,
}

#[derive(Deserialize, serde::Serialize)]
pub(super) struct StopProcessArguments {
    pub(super) runner: String,
    pub(super) process_id: ProcessId,
}

const FOREGROUND_TIMEOUT_MS: u64 = 10_000;

pub(super) enum RunnerOperation {
    Command(ExecCommandArguments),
    Patch(ApplyPatchArguments),
    Wait(WaitProcessArguments),
    Stop(StopProcessArguments),
}

impl RunnerOperation {
    pub(super) fn name(&self) -> &'static str {
        match self {
            Self::Command(_) => "exec_command",
            Self::Patch(_) => "apply_patch",
            Self::Wait(_) => "wait_process",
            Self::Stop(_) => "stop_process",
        }
    }

    pub(super) fn runner(&self) -> &str {
        match self {
            Self::Command(arguments) => &arguments.runner,
            Self::Patch(arguments) => &arguments.runner,
            Self::Wait(arguments) => &arguments.runner,
            Self::Stop(arguments) => &arguments.runner,
        }
    }

    pub(super) fn result_label(&self) -> String {
        match self {
            Self::Command(arguments) => match &arguments.mode {
                ModelCommandMode::Foreground { .. } => "Command".to_owned(),
                ModelCommandMode::Background { process_id } => {
                    format!("Background Command {process_id}")
                }
                ModelCommandMode::Timed { timeout_ms } => {
                    format!("Timed Command {timeout_ms}")
                }
            },
            Self::Patch(_) => "Patch".to_owned(),
            Self::Wait(arguments) => {
                format!("Wait {} {}", arguments.process_id, arguments.timeout_ms)
            }
            Self::Stop(arguments) => format!("Stop {}", arguments.process_id),
        }
    }

    pub(super) fn into_arguments(self) -> ToolArguments {
        match self {
            Self::Command(arguments) => ToolArguments::ExecCommand(arguments),
            Self::Patch(arguments) => ToolArguments::ApplyPatch(arguments),
            Self::Wait(arguments) => ToolArguments::WaitProcess(arguments),
            Self::Stop(arguments) => ToolArguments::StopProcess(arguments),
        }
    }
}

pub(super) fn parse_runner_input(input: &str) -> Result<Vec<RunnerOperation>> {
    let lines = input.lines().collect::<Vec<_>>();
    if lines.first() != Some(&"BEGIN") || lines.last() != Some(&"END") {
        bail!("runner input must start with 'BEGIN' and end with 'END'");
    }
    let lines = &lines[1..lines.len() - 1];
    let mut index = 0;
    let mut runner = None;
    let mut group_operations = 0;
    let mut operations = Vec::new();

    while index < lines.len() {
        if let Some(name) = lines[index].strip_prefix("*** Runner ") {
            if runner.is_some() && group_operations == 0 {
                bail!("runner group must contain at least one operation");
            }
            if name.is_empty() {
                bail!("runner name cannot be empty");
            }
            runner = Some(name.to_owned());
            group_operations = 0;
            index += 1;
            continue;
        }

        let runner = runner
            .as_ref()
            .context("runner input must start with '*** Runner <runner>'")?
            .clone();
        match lines[index] {
            header
                if header == "*** Command"
                    || header.starts_with("*** Background Command ")
                    || header.starts_with("*** Timed Command ") =>
            {
                let mode = match header {
                    "*** Command" => ModelCommandMode::Foreground {
                        timeout_ms: FOREGROUND_TIMEOUT_MS,
                    },
                    header if header.starts_with("*** Background Command ") => {
                        let process_id = &header["*** Background Command ".len()..];
                        if !valid_process_id(process_id) {
                            bail!("invalid background command process ID '{process_id}'");
                        }
                        ModelCommandMode::Background {
                            process_id: ProcessId(process_id.to_owned()),
                        }
                    }
                    header => ModelCommandMode::Timed {
                        timeout_ms: header["*** Timed Command ".len()..]
                            .parse()
                            .context("timed command milliseconds must be an integer")?,
                    },
                };
                index += 1;
                let end = lines[index..]
                    .iter()
                    .position(|line| *line == "*** End")
                    .map(|offset| index + offset)
                    .context("command must end with '*** End'")?;
                if end == index {
                    bail!("command cannot be empty");
                }
                operations.push(RunnerOperation::Command(ExecCommandArguments {
                    runner,
                    command: lines[index..end].join("\n"),
                    mode,
                }));
                group_operations += 1;
                index = end + 1;
            }
            header if header.starts_with("*** Wait ") => {
                let values = &header["*** Wait ".len()..];
                let (process_id, timeout_ms) = values
                    .split_once(' ')
                    .context("wait must include a process ID and timeout")?;
                if !valid_process_id(process_id) {
                    bail!("invalid wait process ID '{process_id}'");
                }
                operations.push(RunnerOperation::Wait(WaitProcessArguments {
                    runner,
                    process_id: ProcessId(process_id.to_owned()),
                    timeout_ms: timeout_ms
                        .parse()
                        .context("wait timeout must be an integer")?,
                }));
                group_operations += 1;
                index += 1;
            }
            header if header.starts_with("*** Stop ") => {
                let process_id = &header["*** Stop ".len()..];
                if !valid_process_id(process_id) {
                    bail!("invalid stop process ID '{process_id}'");
                }
                operations.push(RunnerOperation::Stop(StopProcessArguments {
                    runner,
                    process_id: ProcessId(process_id.to_owned()),
                }));
                group_operations += 1;
                index += 1;
            }
            "*** Patch" => {
                index += 1;
                let end = lines[index..]
                    .iter()
                    .position(|line| *line == "*** End")
                    .map(|offset| index + offset)
                    .context("patch must end with '*** End'")?;
                if end == index {
                    bail!("patch cannot be empty");
                }
                let patch = lines[index..end].join("\n");
                operations.push(RunnerOperation::Patch(ApplyPatchArguments {
                    runner,
                    patch,
                }));
                group_operations += 1;
                index = end + 1;
            }
            line => bail!("expected runner operation, got '{line}'"),
        }
    }

    if runner.is_none() {
        bail!("runner input must contain at least one runner group");
    }
    if group_operations == 0 {
        bail!("runner group must contain at least one operation");
    }
    Ok(operations)
}

pub(super) fn valid_process_id(process_id: &str) -> bool {
    process_id.len() <= 64
        && process_id
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && process_id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

#[derive(serde::Serialize)]
#[serde(untagged)]
pub(super) enum ToolArguments {
    ExecCommand(ExecCommandArguments),
    ApplyPatch(ApplyPatchArguments),
    WaitProcess(WaitProcessArguments),
    StopProcess(StopProcessArguments),
}

pub(super) struct ToolOutcome {
    pub(super) result: serde_json::Value,
    pub(super) artifacts: Vec<ToolArtifact>,
}

impl ToolOutcome {
    pub(super) fn text(result: String) -> Self {
        Self {
            result: serde_json::Value::String(result),
            artifacts: Vec::new(),
        }
    }

    pub(super) fn with_artifact(result: String, artifact: ToolArtifact) -> Self {
        Self {
            result: serde_json::Value::String(result),
            artifacts: vec![artifact],
        }
    }
}

impl ToolArguments {
    pub(super) fn runner(&self) -> &str {
        match self {
            Self::ExecCommand(arguments) => &arguments.runner,
            Self::ApplyPatch(arguments) => &arguments.runner,
            Self::WaitProcess(arguments) => &arguments.runner,
            Self::StopProcess(arguments) => &arguments.runner,
        }
    }

    pub(super) fn requires_approval(&self) -> bool {
        matches!(self, Self::ExecCommand(_) | Self::ApplyPatch(_))
    }
}

pub(super) struct OperationContext {
    pub(super) call_id: String,
    pub(super) index: usize,
    pub(super) label: String,
}

pub(super) fn send_operation_update(
    operation: Option<&OperationContext>,
    updates: Option<&mpsc::UnboundedSender<ModelStreamEvent>>,
    update: RunnerOperationUpdate,
) -> Result<()> {
    if let Some(operation) = operation {
        updates
            .context("runner operation update requires a streaming turn")?
            .send(ModelStreamEvent::RunnerOperationUpdate {
                call_id: operation.call_id.clone(),
                operation_index: operation.index,
                update,
            })
            .context("turn stream closed during runner operation")?;
    }
    Ok(())
}

pub(super) fn format_patch_result(result: &ApplyPatchResult) -> String {
    let results = match result {
        ApplyPatchResult::ParseError { error } => {
            return format!("apply_patch failed:\n{error}");
        }
        ApplyPatchResult::Operations { results } => results,
    };
    let failed = results.iter().any(|result| {
        matches!(
            result,
            PatchOperationResult::Added {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            } | PatchOperationResult::Deleted {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            } | PatchOperationResult::Updated {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            } | PatchOperationResult::Moved {
                outcome: PatchOperationOutcome::Failed { .. },
                ..
            }
        )
    });
    let mut output = if failed {
        String::from("apply_patch completed with errors:\n")
    } else {
        String::from("Success. Updated the following files:\n")
    };
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
            PatchOperationOutcome::Applied { .. } => output.push_str(&format!("{label}\n")),
            PatchOperationOutcome::Failed { error } => {
                output.push_str(&format!("{label}: {error}\n"));
            }
        }
    }
    output
}

pub(super) fn masked_tool_result(payload: &ToolResultEvent) -> Option<String> {
    let (original, artifacts, custom) = match payload {
        ToolResultEvent::Custom {
            result, artifacts, ..
        } => (result.as_str()?, artifacts, true),
        ToolResultEvent::Function {
            result, artifacts, ..
        } => (result.as_str()?, artifacts, false),
    };
    if custom {
        let mut operations = Vec::new();
        let mut command_found = false;
        let mut command_masked = false;
        for artifact in artifacts {
            let ToolArtifact::RunnerOperation(data) = artifact else {
                continue;
            };
            let operation_result = data.result.as_str()?;
            let masked = data.artifacts.iter().find_map(|artifact| match artifact {
                ToolArtifact::CommandExecution(command) => masked_command_result(command),
                ToolArtifact::PatchOperations(_) | ToolArtifact::RunnerOperation(_) => None,
            });
            let result = if let Some(masked) = masked {
                command_found = true;
                if model::text_tokens(&masked) < model::text_tokens(operation_result) {
                    command_masked = true;
                    masked
                } else {
                    operation_result.to_owned()
                }
            } else {
                operation_result.to_owned()
            };
            operations.push(format!(
                "Operation {} [{}] {}:\n{result}",
                data.operation, data.runner, data.label
            ));
        }
        if !command_found {
            return None;
        }
        let masked = operations.join("\n\n");
        return (command_masked && model::text_tokens(&masked) < model::text_tokens(original))
            .then_some(masked);
    }

    let command = artifacts.iter().find_map(|artifact| match artifact {
        ToolArtifact::CommandExecution(command) => Some(command),
        ToolArtifact::PatchOperations(_) | ToolArtifact::RunnerOperation(_) => None,
    })?;
    let masked = masked_command_result(command)?;
    (model::text_tokens(&masked) < model::text_tokens(original)).then_some(masked)
}

pub(super) fn masked_command_result(command: &CommandExecutionArtifact) -> Option<String> {
    let (output, runner, full_output_path, status) = match command {
        CommandExecutionArtifact::Started { .. } => return None,
        CommandExecutionArtifact::Running {
            output,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            "Process is still running".to_owned(),
        ),
        CommandExecutionArtifact::Finished {
            output,
            exit_code,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            format!(
                "Process exited with code {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
        ),
        CommandExecutionArtifact::TimedOut {
            output,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            "Process timed out".to_owned(),
        ),
        CommandExecutionArtifact::Stopped {
            output,
            runner,
            full_output_path,
        } => (
            output,
            runner,
            full_output_path,
            "Process stopped".to_owned(),
        ),
    };
    if output.is_empty() {
        return None;
    }
    let lines = output.lines().collect::<Vec<_>>();
    let head = lines
        .iter()
        .take(MASK_OUTPUT_LINES)
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let head = truncate_mask_head(&head);
    let tail = lines
        .iter()
        .rev()
        .take(MASK_OUTPUT_LINES)
        .copied()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let tail = truncate_mask_tail(&tail);
    Some(clean_model_output(&format!(
        "{head}\n\n... output masked ...\n\n{tail}\n\n{status}\n\
         Full output: runner \"{runner}\": {}",
        full_output_path.display()
    )))
}

pub(super) fn truncate_mask_head(output: &str) -> &str {
    &output[..floor_char_boundary(output, output.len().min(MASK_OUTPUT_SIDE_BYTES))]
}

pub(super) fn truncate_mask_tail(output: &str) -> &str {
    &output[ceil_char_boundary(output, output.len().saturating_sub(MASK_OUTPUT_SIDE_BYTES))..]
}

pub(super) fn command_artifact(
    response: &RunnerResponse,
    runner: &str,
) -> Result<CommandExecutionArtifact> {
    Ok(match response {
        RunnerResponse::ProcessStarted { .. } => CommandExecutionArtifact::Started {
            runner: runner.to_owned(),
        },
        RunnerResponse::ProcessRunning { output, .. } => CommandExecutionArtifact::Running {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
        RunnerResponse::ProcessFinished { output, exit_code } => {
            CommandExecutionArtifact::Finished {
                output: format_command_output(output),
                exit_code: *exit_code,
                runner: runner.to_owned(),
                full_output_path: output.full_output_path.clone(),
            }
        }
        RunnerResponse::ProcessTimedOut { output } => CommandExecutionArtifact::TimedOut {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
        RunnerResponse::ProcessStopped { output } => CommandExecutionArtifact::Stopped {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
        RunnerResponse::Error { message } => bail!("{message}"),
        _ => bail!("runner returned an invalid command response"),
    })
}

pub(super) fn format_process_response(
    tool: &str,
    runner: &str,
    response: RunnerResponse,
) -> Result<String> {
    match response {
        RunnerResponse::ProcessRunning {
            process_handle,
            output,
        } => Ok(append_process_status(
            model_command_output(&output, runner),
            &format!("Process {process_handle} is still running"),
        )),
        RunnerResponse::ProcessFinished {
            output,
            exit_code: Some(0),
        } => {
            let output = model_command_output(&output, runner);
            Ok(if output.is_empty() {
                "Process completed with no output".to_owned()
            } else {
                output
            })
        }
        RunnerResponse::ProcessFinished { output, exit_code } => Ok(append_process_status(
            model_command_output(&output, runner),
            &format!(
                "Process exited with code {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
        )),
        RunnerResponse::ProcessStopped { output } => Ok(append_process_status(
            model_command_output(&output, runner),
            "Process stopped",
        )),
        RunnerResponse::Error { message } => bail!("{message}"),
        _ => bail!("runner returned an invalid {tool} response"),
    }
}

pub(super) fn append_command_output(collected: &mut Option<CommandOutput>, output: CommandOutput) {
    match collected {
        Some(collected) => {
            collected.content.push_str(&output.content);
            collected.omitted_bytes += output.omitted_bytes;
            collected.full_output_path = output.full_output_path;
            if collected.content.len() > MAX_TOOL_OUTPUT_BYTES {
                let head_end = floor_char_boundary(&collected.content, MAX_TOOL_OUTPUT_BYTES / 2);
                let tail_start = ceil_char_boundary(
                    &collected.content,
                    collected.content.len() - MAX_TOOL_OUTPUT_BYTES / 2,
                )
                .max(head_end);
                collected.omitted_bytes += tail_start - head_end;
                collected.content.replace_range(head_end..tail_start, "");
            }
        }
        None => *collected = Some(output),
    }
}

const MAX_TOOL_OUTPUT_BYTES: usize = 40_000;

pub(super) fn format_command_output(output: &CommandOutput) -> String {
    format_command_output_with_location(output, &output.full_output_path.display().to_string())
}

pub(super) fn format_command_output_with_location(
    output: &CommandOutput,
    full_output_location: &str,
) -> String {
    if output.omitted_bytes == 0 && output.content.len() <= MAX_TOOL_OUTPUT_BYTES {
        return output.content.clone();
    }

    let head_end = floor_char_boundary(
        &output.content,
        (MAX_TOOL_OUTPUT_BYTES / 2).min(output.content.len()),
    );
    let tail_start = ceil_char_boundary(
        &output.content,
        output
            .content
            .len()
            .saturating_sub(MAX_TOOL_OUTPUT_BYTES - MAX_TOOL_OUTPUT_BYTES / 2),
    )
    .max(head_end);
    let omitted_bytes = output.omitted_bytes + tail_start.saturating_sub(head_end);
    format!(
        "{}\n\n... {omitted_bytes} bytes omitted; full output: {full_output_location} ...\n\n{}",
        &output.content[..head_end],
        &output.content[tail_start..]
    )
}

pub(super) fn model_command_output(output: &CommandOutput, runner: &str) -> String {
    let location = format!("runner \"{runner}\": {}", output.full_output_path.display());
    clean_model_output(&format_command_output_with_location(output, &location))
}

pub(super) fn clean_model_output(output: &str) -> String {
    output
        .chars()
        .filter(|character| {
            matches!(character, '\t' | '\n' | '\r')
                || (*character >= ' ' && !('\u{fff9}'..='\u{fffb}').contains(character))
        })
        .filter(|character| *character != '\r')
        .collect()
}

pub(super) fn append_process_status(output: String, status: &str) -> String {
    if output.is_empty() {
        status.to_owned()
    } else {
        format!("{output}\n\n{status}")
    }
}

pub(super) fn floor_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index -= 1;
    }
    index
}

pub(super) fn ceil_char_boundary(value: &str, mut index: usize) -> usize {
    while !value.is_char_boundary(index) {
        index += 1;
    }
    index
}
