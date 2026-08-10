use super::*;
use indoc::indoc;

pub(super) fn model_tools() -> Vec<model::ModelTool> {
    vec![
        model::ModelTool::WebSearch,
        model::ModelTool::Custom {
            name: "command",
            description: indoc! {"
                Execute one or more Bash scripts on named Atra Runners.
                Start each script with `*** Runner <runner>`; repeat it to run another script or switch Runners.
                A script ends at the next `*** Runner <runner>` line or the end of the tool input.
                Scripts run with `bash -lc` without implicit `set -e`.
                Use `set -e`, `&&`, or `|| exit 1` when later commands must not run after a failure.
                Runner scripts execute sequentially; a non-zero exit status does not prevent later Runner scripts from running.

                Processes:
                Each command waits up to 120000 milliseconds. If it is still running, it is detached and returned as a managed process.
                Process IDs are local to each Runner within the current conversation and must match `[a-z][a-z0-9_-]{0,63}`.
                A process ID reported after a foreground timeout can be passed directly to `atri proc wait` or `atri proc stop`; do not rerun the command.
                Run `atri proc spawn <process-id> '<command>'` to start a named managed process without waiting.
                Run `atri proc wait <process-id>... [--timeout <seconds>]` to wait for all named processes. The timeout defaults to 10 seconds and may not exceed 60 seconds.
                Run `atri proc stop <process-id>...` to stop named processes.
                These commands report every process in argument order. A wait timeout reports processes as running and does not fail.

                Patches:
                Run `atri patch` and pass the patch on standard input to add, update, delete, or move files.
                Use a quoted Bash heredoc ending the command line with `atri patch <<'PATCH'` and terminate it with `PATCH` on its own line.
                Preceding and following commands may be joined to `atri patch` with shell operators. A typical invocation is:
                `atri patch <<'PATCH'`
                `*** Begin Patch`
                `...`
                `*** End Patch`
                `PATCH`
                Patch hunks start with `*** Add File: <path>`, `*** Update File: <path>`, or `*** Delete File: <path>`; a move follows an update header with `*** Move to: <path>` and may omit change lines when the contents are unchanged.
                Enclose the hunks with `*** Begin Patch` and `*** End Patch` on their own lines.
                Paths in patches are relative to the command's working directory unless absolute.
                Use line ranges for large deletions or replacements when the line numbers are already known.
                When inspecting a file is otherwise necessary, obtain line numbers as part of that inspection.
                Use ordinary diff lines for small changes.
                Do not make an additional operation solely to obtain line numbers unless doing so avoids a substantially larger patch.

                Replacements:
                Run `atri replace <path>` and pass exact old and new text on standard input:
                `atri replace path/to/file <<'REPLACE'`
                `*** Old`
                `old text`
                `*** New`
                `new text`
                `REPLACE`
                The replacement fails without changing the file unless the old text occurs exactly once.
                Use `atri replace --all <path>` to replace every occurrence.

                Commands in one tool call execute sequentially, and their results are returned together after all commands have finished.
                Use a separate tool call when a result is needed to decide the next operation.
            "},
            format: model::ModelToolFormat {
                syntax: "lark",
                definition: indoc! {r#"
                    start: runner_script+
                    runner_script: runner command_item+
                    runner: "*** Runner " name LF

                    ?command_item: command_line | patch | replace
                    command_line: /([^*].*|\*[^*].*|\*\*[^*].*|\*\*\*[^ ].*|\*|\*\*|\*\*\*)/ LF? | LF

                    patch: PATCH_COMMAND_LINE LF PATCH_BEGIN LF hunk+ PATCH_END LF "PATCH" LF?
                    PATCH_COMMAND_LINE: /([^\n]*[;&|][ \t]*)?[ \t]*atri[ \t]+patch[ \t]*<<'PATCH'([ \t]*(&&|\|\||;)[ \t]*[^\n]+)?/
                    PATCH_BEGIN: "*** Begin Patch"
                    PATCH_END: "*** End Patch"
                    hunk: add_hunk | delete_hunk | update_hunk
                    add_hunk: "*** Add File: " filename LF add_line+
                    delete_hunk: "*** Delete File: " filename LF
                    update_hunk: "*** Update File: " filename LF (change_move move_changes? | first_update following_update*)

                    name: /(.+)/
                    filename: /(.+)/
                    add_line: "+" /(.*)/ LF -> line

                    change_move: "*** Move to: " filename LF
                    move_changes: first_update following_update*
                    first_update: change | range_change
                    following_update: headed_change | range_change
                    change: change_context? change_line+ eof_line?
                    headed_change: change_context change_line+ eof_line?
                    change_context: ("@@" | "@@ " /(.+)/) LF
                    change_line: ("+" | "-" | " ") /(.*)/ LF
                    eof_line: "*** End of File" LF

                    range_change: range_start remove_line (range_end remove_line)? add_line*
                    range_start: "@ start " INT LF
                    range_end: "@ end " INT LF
                    remove_line: "-" /(.*)/ LF

                    replace: REPLACE_COMMAND_LINE LF "*** Old" LF replace_old "*** New" LF replace_new "REPLACE" LF?
                    REPLACE_COMMAND_LINE: /([^\n]*[;&|][ \t]*)?[ \t]*atri[ \t]+replace([ \t]+--all)?[ \t]+[^<\n]+<<'REPLACE'([ \t]*(&&|\|\||;)[ \t]*[^\n]+)?/
                    replace_old: replace_old_line*
                    replace_new: replace_new_line*
                    replace_old_line: /(?!\*\*\* New\n)[^\n]+/ LF | LF
                    replace_new_line: /(?!REPLACE(?:\n|$))[^\n]+/ LF | LF

                    %import common.INT
                    %import common.LF
                "#},
            },
        },
    ]
}

#[derive(Deserialize, serde::Serialize)]
pub(super) struct CommandArguments {
    pub(super) runner: String,
    pub(super) command: String,
}

pub(super) fn parse_todo_annotation(content: String) -> (String, Vec<TodoItem>) {
    const OPEN: &str = "<todo>\n";
    const CLOSE: &str = "\n</todo>";
    let Some(body) = content.strip_prefix(OPEN) else {
        return (content, Vec::new());
    };
    let Some(close) = body.find(CLOSE) else {
        return (content, Vec::new());
    };
    let todo_text = &body[..close];
    let remainder = &body[close + CLOSE.len()..];
    if !remainder.is_empty() && !remainder.starts_with('\n') {
        return (content, Vec::new());
    }

    let mut todos = Vec::new();
    for line in todo_text.lines() {
        let (status, step) = if let Some(step) = line.strip_prefix("- [x]: ") {
            (TodoStatus::Completed, step)
        } else if let Some(step) = line.strip_prefix("- [-]: ") {
            (TodoStatus::InProgress, step)
        } else if let Some(step) = line.strip_prefix("- [ ]: ") {
            (TodoStatus::Pending, step)
        } else {
            return (content, Vec::new());
        };
        let step = step.trim();
        if step.is_empty() {
            return (content, Vec::new());
        }
        todos.push(TodoItem {
            step: step.to_owned(),
            status,
        });
    }
    if todos.is_empty() {
        return (content, Vec::new());
    }

    let remainder = remainder
        .strip_prefix("\n\n")
        .or_else(|| remainder.strip_prefix('\n'))
        .unwrap_or(remainder);
    (remainder.to_owned(), todos)
}

pub(super) const FOREGROUND_TIMEOUT_MS: u64 = 120_000;

impl CommandArguments {
    pub(super) fn name(&self) -> &'static str {
        "command"
    }

    pub(super) fn runner(&self) -> &str {
        &self.runner
    }

    pub(super) fn result_label(&self) -> String {
        "Command".to_owned()
    }
}

pub(super) fn parse_command_input(input: &str) -> Result<Vec<CommandArguments>> {
    let lines = input.lines().collect::<Vec<_>>();
    let mut runner = None;
    let mut command_start = 0;
    let mut operations = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = line.strip_prefix("*** Runner ") {
            if let Some(runner) = runner.replace(name.to_owned()) {
                if command_start == index {
                    bail!("runner group must contain a command");
                }
                operations.push(CommandArguments {
                    runner,
                    command: lines[command_start..index].join("\n"),
                });
            }
            if name.is_empty() {
                bail!("runner name cannot be empty");
            }
            command_start = index + 1;
        } else if runner.is_none() {
            bail!("command input must start with '*** Runner <runner>'");
        }
    }

    let runner = runner.context("command input must contain at least one runner group")?;
    if command_start == lines.len() {
        bail!("runner group must contain a command");
    }
    operations.push(CommandArguments {
        runner,
        command: lines[command_start..].join("\n"),
    });
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
    response: &CommandOutcome,
    runner: &str,
) -> CommandExecutionArtifact {
    match response {
        CommandOutcome::Running { output, .. } => CommandExecutionArtifact::Running {
            output: format_command_output(output),
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
        CommandOutcome::Finished {
            output, exit_code, ..
        } => CommandExecutionArtifact::Finished {
            output: format_command_output(output),
            exit_code: *exit_code,
            runner: runner.to_owned(),
            full_output_path: output.full_output_path.clone(),
        },
    }
}

pub(super) fn format_command_response(runner: &str, response: CommandOutcome) -> String {
    match response {
        CommandOutcome::Running {
            process_id, output, ..
        } => append_process_status(
            model_command_output(&output, runner),
            &format!(
                "Foreground timeout reached after {FOREGROUND_TIMEOUT_MS} milliseconds. \
                 The command was detached and remains managed.\n\
                 Process ID: {process_id}\n\
                 Continue with: `atri proc wait {process_id} --timeout 60`\n\
                 Stop with: `atri proc stop {process_id}`\n\
                 Do not rerun the command."
            ),
        ),
        CommandOutcome::Finished {
            output,
            exit_code: Some(0),
            ..
        } => {
            let output = model_command_output(&output, runner);
            if output.is_empty() {
                "Process completed with no output".to_owned()
            } else {
                output
            }
        }
        CommandOutcome::Finished {
            output, exit_code, ..
        } => append_process_status(
            model_command_output(&output, runner),
            &format!(
                "Process exited with code {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
        ),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_todo_annotation() {
        let (content, todos) = parse_todo_annotation(
            "<todo>\n- [x]: task a\n- [-]: task b\n- [ ]: task c\n</todo>\n\nBody".to_owned(),
        );
        assert_eq!(content, "Body");
        assert_eq!(
            todos,
            vec![
                TodoItem {
                    step: "task a".to_owned(),
                    status: TodoStatus::Completed,
                },
                TodoItem {
                    step: "task b".to_owned(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    step: "task c".to_owned(),
                    status: TodoStatus::Pending,
                },
            ]
        );
    }

    #[test]
    fn preserves_invalid_todo_annotation() {
        for content in [
            "<todo>\n- [!]: task\n</todo>",
            "<todo>\n- [ ]: \n</todo>",
            "<todo>\n- [ ]: task",
            "Body\n<todo>\n- [ ]: task\n</todo>",
        ] {
            let (parsed, todos) = parse_todo_annotation(content.to_owned());
            assert_eq!(parsed, content);
            assert!(todos.is_empty());
        }
    }

    #[test]
    fn parses_multiple_in_progress_todos() {
        let (content, todos) =
            parse_todo_annotation("<todo>\n- [-]: one\n- [-]: two\n</todo>".to_owned());
        assert!(content.is_empty());
        assert_eq!(todos.len(), 2);
        assert!(
            todos
                .iter()
                .all(|todo| todo.status == TodoStatus::InProgress)
        );
    }
}
