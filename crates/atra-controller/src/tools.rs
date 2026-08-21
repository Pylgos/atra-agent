use super::*;
use indoc::indoc;

pub(super) fn model_tools(allow_questions: bool) -> Vec<model::ModelTool> {
    let mut tools = vec![model::ModelTool::WebSearch];
    if allow_questions {
        tools.push(model::ModelTool::Tool {
            name: "question",
            json: Some(model::ModelJsonToolInterface {
                description: "Ask the user one or more questions. Each question is answered with one option and an optional free-form note. recommended_options must contain option labels. The UI adds a final \"どれでもない\" option automatically, so do not provide it; that answer is returned with selected_option null. Use this when user input is required before continuing.".to_owned(),
                parameters: question_parameters(),
            }),
            custom: None,
        });
    }
    let command_description = indoc! {"
        Execute Bash scripts on named Atra Runners.
        Scripts run with `bash -lc` without implicit `set -e`.
        Use `set -e`, `&&`, or `|| exit 1` when later commands must not run after a failure.

        Processes:
        Each command waits up to 120 seconds. If it is still running, it is detached and returned as a managed process.
        Process IDs are local to each Runner within the current conversation and must match `[a-z][a-z0-9_-]{0,63}`.
        A process ID reported after a foreground timeout can be passed directly to `atri proc wait` or `atri proc stop`; do not rerun the command.
        Run `atri proc spawn <process-id> '<command>'` to start a named managed process without waiting.
        Run `atri proc wait <process-id>... [--timeout <seconds>]` to wait for all named processes. The timeout defaults to 120 seconds and has no configured maximum.
        While `atri proc wait` is running, the calling command's detach timer stops. It resumes with its remaining time when the wait ends.
        Run `atri proc stop <process-id>...` to stop named processes.
        These commands report every process in argument order. A wait timeout reports processes as running and does not fail.

        Subagents:
        `atri agent create --name <name> [--model <provider>/<model>] [--effort <effort>] [--allow-delegation]` creates an empty child thread without copying this conversation.
        Omit `--model` and `--effort` normally; omitted values are inherited from the parent thread. Override them only when explicitly required by the user or applicable instructions.
        `--allow-delegation` permits that child to create its own children. A child without this permission is rejected by the Controller if it attempts `agent create`.
        Use `atri agent send <thread-id> [<message>]`, `atri agent wait [--timeout <seconds>] <thread-id>@<after-sequence>...`, `atri agent list`, `atri agent cancel [--recursive] <thread-id>...`, and `atri agent delete [--recursive] <thread-id>...`.
        Automated child turns cannot ask questions, remain alive independently of the parent turn, and are limited to eight concurrently running descendants per root.
        For the first `wait` after `send`, use the `after_sequence` returned by `send`. For subsequent waits, use the `through` returned by the previous `wait`. Use `-1` only when intentionally reading the thread from its beginning.
        Before completing the parent turn, stop every descendant that is still running.

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
        Run `atri replace <path>` with the exact old text on file descriptor 3 and new text on file descriptor 4:
        `atri replace path/to/file 3<<'OLD' 4<<'NEW'`
        `old text`
        `OLD`
        `new text`
        `NEW`
        A single trailing newline is removed from each heredoc. Add a blank line before its delimiter when the text must end with a newline.
        For example, this replaces `old text\n` with `new text`:
        `atri replace path/to/file 3<<'OLD' 4<<'NEW'`
        `old text`
        ``
        `OLD`
        `new text`
        `NEW`
        This replaces `old text` with an empty string:
        `atri replace path/to/file 3<<'OLD' 4<<'NEW'`
        `old text`
        `OLD`
        `NEW`
        The replacement fails without changing the file unless the old text occurs exactly once.
        Use `atri replace --all <path>` to replace every occurrence.

        Use a separate tool call when a result is needed to decide the next operation.
    "};
    tools.extend([
        model::ModelTool::Tool {
            name: "command",
            json: Some(model::ModelJsonToolInterface {
                description: interface_description(
                    command_description,
                    "Execute exactly one Bash script. Set `runner` to the Runner name and `command` to the complete script.",
                ),
                parameters: command_parameters(),
            }),
            custom: Some(model::ModelCustomToolInterface {
                description: interface_description(
                    command_description,
                    indoc! {"
                        Execute one or more Bash scripts.
                        Start each script with `*** Runner <runner>`; repeat it to run another script or switch Runners.
                        A script ends at the next `*** Runner <runner>` line or the end of the tool input.
                        Runner scripts execute sequentially; a non-zero exit status does not prevent later Runner scripts from running.
                        Their results are returned together after all scripts have finished.
                    "},
                ),
                format: model::ModelToolFormat {
                    syntax: "lark",
                    // Codex's Lark parser does not support regex lookaround such as `(?!...)`.
                    definition: indoc! {r#"
                    start: runner_script+
                    runner_script: runner command_item+
                    runner: "*** Runner " name LF

                    ?command_item: command_line | patch
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

                    %import common.INT
                    %import common.LF
                    "#},
                },
            }),
        },
    ]);
    tools
}

fn interface_description(description: &str, instructions: &str) -> String {
    format!("{}\n\n{}", description.trim_end(), instructions.trim())
}

pub(super) fn question_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "properties": {
                        "question": {"type": "string", "minLength": 1},
                        "options": {
                            "type": "array",
                            "minItems": 1,
                            "items": {
                                "type": "object",
                                "properties": {
                                    "label": {"type": "string", "minLength": 1},
                                    "description": {"type": "string"}
                                },
                                "required": ["label", "description"],
                                "additionalProperties": false
                            }
                        },
                        "recommended_options": {
                            "type": "array",
                            "items": {"type": "string"}
                        }
                    },
                    "required": ["question", "options", "recommended_options"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["questions"],
        "additionalProperties": false
    })
}

fn command_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "runner": {
                "type": "string",
                "minLength": 1,
                "description": "The name of the Atra Runner that executes the script."
            },
            "command": {
                "type": "string",
                "minLength": 1,
                "description": "The complete Bash script to execute with `bash -lc`."
            }
        },
        "required": ["runner", "command"],
        "additionalProperties": false
    })
}

pub(super) fn web_search_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "query": {"type": "string"},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 10}
        },
        "required": ["query"]
    })
}

pub(super) fn web_fetch_parameters() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"url": {"type": "string"}},
        "required": ["url"]
    })
}

#[derive(Debug)]
pub(super) struct ToolInputError {
    reason: String,
    expected: Option<String>,
}

impl ToolInputError {
    fn json(reason: impl Into<String>, schema: serde_json::Value) -> Self {
        Self {
            reason: reason.into(),
            expected: Some(format!("Expected schema:\n{schema}")),
        }
    }

    fn grammar(reason: impl Into<String>, syntax: &str, definition: &str) -> Self {
        Self {
            reason: reason.into(),
            expected: Some(format!("Expected {syntax} grammar:\n{definition}")),
        }
    }

    fn unavailable() -> Self {
        Self {
            reason: "tool is not available in this turn".to_owned(),
            expected: None,
        }
    }

    pub(super) fn tool_result(&self, name: &str) -> String {
        let Some(expected) = &self.expected else {
            return format!(
                "Tool call rejected for `{name}`.\n\n\
                 Error: {}.\n\n\
                 Retry using one of the tools presented in this turn.",
                self.reason
            );
        };
        format!(
            "Tool input schema violation for `{name}`.\n\n\
             Error: {}.\n\n\
             {expected}\n\n\
             Retry the tool call using input that matches this definition.",
            self.reason
        )
    }
}

#[derive(Debug)]
pub(super) enum ValidatedFunctionTool {
    Questions(Vec<atra_protocol::Question>),
    Command(atra_protocol::RunnerCommand),
    Provider,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArguments {
    runner: String,
    command: String,
}

#[derive(Deserialize)]
struct WebSearchArguments {
    #[serde(rename = "query")]
    _query: String,
    max_results: Option<u8>,
}

#[derive(Deserialize)]
struct WebFetchArguments {
    #[serde(rename = "url")]
    _url: String,
}

pub(super) fn validate_function_tool(
    name: &str,
    arguments: serde_json::Value,
    allow_questions: bool,
) -> std::result::Result<ValidatedFunctionTool, ToolInputError> {
    match name {
        "question" if allow_questions => parse_questions(arguments)
            .map(ValidatedFunctionTool::Questions)
            .map_err(|error| ToolInputError::json(format!("{error:#}"), question_parameters())),
        "command" => {
            let arguments: CommandArguments = serde_json::from_value(arguments)
                .map_err(|error| ToolInputError::json(error.to_string(), command_parameters()))?;
            if arguments.runner.trim().is_empty() {
                return Err(ToolInputError::json(
                    "property `runner` must not be empty",
                    command_parameters(),
                ));
            }
            if arguments.command.trim().is_empty() {
                return Err(ToolInputError::json(
                    "property `command` must not be empty",
                    command_parameters(),
                ));
            }
            Ok(ValidatedFunctionTool::Command(
                atra_protocol::RunnerCommand::from_parts(arguments.runner, arguments.command),
            ))
        }
        "web_search" => {
            let arguments: WebSearchArguments =
                serde_json::from_value(arguments).map_err(|error| {
                    ToolInputError::json(error.to_string(), web_search_parameters())
                })?;
            if arguments
                .max_results
                .is_some_and(|value| !(1..=10).contains(&value))
            {
                return Err(ToolInputError::json(
                    "property `max_results` must be an integer between 1 and 10",
                    web_search_parameters(),
                ));
            }
            Ok(ValidatedFunctionTool::Provider)
        }
        "web_fetch" => {
            let _: WebFetchArguments = serde_json::from_value(arguments)
                .map_err(|error| ToolInputError::json(error.to_string(), web_fetch_parameters()))?;
            Ok(ValidatedFunctionTool::Provider)
        }
        _ => Err(ToolInputError::unavailable()),
    }
}

pub(super) fn validate_custom_tool(
    name: &str,
    input: &str,
) -> std::result::Result<Vec<atra_protocol::RunnerCommand>, ToolInputError> {
    let Some(format) = model_tools(false).into_iter().find_map(|tool| match tool {
        model::ModelTool::Tool {
            name: candidate,
            custom: Some(custom),
            ..
        } if candidate == name => Some(custom.format),
        _ => None,
    }) else {
        return Err(ToolInputError::unavailable());
    };
    atra_protocol::parse_command_input(input).map_err(|error| {
        ToolInputError::grammar(error.to_string(), format.syntax, format.definition)
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QuestionArguments {
    questions: Vec<atra_protocol::Question>,
}

pub(super) fn parse_questions(
    arguments: serde_json::Value,
) -> Result<Vec<atra_protocol::Question>> {
    let mut arguments: QuestionArguments =
        serde_json::from_value(arguments).context("invalid question arguments")?;
    if arguments.questions.is_empty() {
        bail!("question tool requires at least one question");
    }
    for question in &mut arguments.questions {
        if question.question.trim().is_empty() {
            bail!("question text must not be empty");
        }
        if question.options.is_empty() {
            bail!("each question requires at least one option");
        }
        let mut labels = HashSet::new();
        for option in &question.options {
            if option.label.trim().is_empty() {
                bail!("option labels must not be empty");
            }
            if option.label == "どれでもない" {
                bail!("the reserved option label どれでもない must not be provided");
            }
            if !labels.insert(option.label.as_str()) {
                bail!("option labels must be unique within a question");
            }
        }
        let mut recommended = HashSet::new();
        for label in &question.recommended_options {
            if !labels.contains(label.as_str()) {
                bail!("recommended_options contains an unknown option");
            }
            if !recommended.insert(label) {
                bail!("recommended_options must not contain duplicates");
            }
        }
        question
            .options
            .sort_by_key(|option| !question.recommended_options.contains(&option.label));
    }
    Ok(arguments.questions)
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

const FOREGROUND_TIMEOUT_SECONDS: u64 = 120;
pub(super) const FOREGROUND_TIMEOUT_MS: u64 = FOREGROUND_TIMEOUT_SECONDS * 1000;

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

pub(super) async fn send_operation_update(
    operation: &OperationContext,
    updates: Option<&TurnProjector>,
    runner: &str,
    update: RunnerOperationUpdate,
) -> Result<()> {
    updates
        .context("runner operation update requires a streaming turn")?
        .apply_update(ModelStreamEvent::RunnerOperationUpdate {
            call_id: operation.call_id.clone(),
            operation_index: operation.index,
            runner: runner.to_owned(),
            update,
        })
        .await?;
    Ok(())
}

pub(super) async fn send_operation_output(
    operation: &OperationContext,
    updates: Option<&TurnProjector>,
    content: String,
    omitted_bytes: usize,
    timer: CommandTimerState,
) -> Result<()> {
    updates
        .context("runner operation output requires a streaming turn")?
        .apply_update(ModelStreamEvent::RunnerOperationOutput {
            call_id: operation.call_id.clone(),
            operation_index: operation.index,
            content,
            omitted_bytes,
            timer,
        })
        .await?;
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
                "Foreground timeout reached after {FOREGROUND_TIMEOUT_SECONDS} seconds. \
                 The command was detached and remains managed.\n\
                 Process ID: {process_id}\n\
                 Continue with: `atri proc wait {process_id}`\n\
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
            if collected.content.len() > MAX_COMMAND_OUTPUT_BYTES {
                let head_end =
                    floor_char_boundary(&collected.content, MAX_COMMAND_OUTPUT_BYTES / 2);
                let tail_start = ceil_char_boundary(
                    &collected.content,
                    collected.content.len() - MAX_COMMAND_OUTPUT_BYTES / 2,
                )
                .max(head_end);
                collected.omitted_bytes += tail_start - head_end;
                collected.content.replace_range(head_end..tail_start, "");
            }
        }
        None => *collected = Some(output),
    }
}

pub(super) fn format_command_output(output: &CommandOutput) -> String {
    format_command_output_with_location(output, &output.full_output_path.display().to_string())
}

pub(super) fn format_command_output_with_location(
    output: &CommandOutput,
    full_output_location: &str,
) -> String {
    if output.omitted_bytes == 0 && output.content.len() <= MAX_COMMAND_OUTPUT_BYTES {
        return output.content.clone();
    }

    let head_end = floor_char_boundary(
        &output.content,
        (MAX_COMMAND_OUTPUT_BYTES / 2).min(output.content.len()),
    );
    let tail_start = ceil_char_boundary(
        &output.content,
        output
            .content
            .len()
            .saturating_sub(MAX_COMMAND_OUTPUT_BYTES - MAX_COMMAND_OUTPUT_BYTES / 2),
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
    fn question_tool_is_only_available_for_interactive_turns() {
        assert!(model_tools(true).iter().any(|tool| matches!(
            tool,
            model::ModelTool::Tool {
                name: "question",
                json: Some(_),
                ..
            }
        )));
        assert!(model_tools(false).iter().all(|tool| !matches!(
            tool,
            model::ModelTool::Tool {
                name: "question",
                ..
            }
        )));
    }

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

    #[test]
    fn parses_valid_questions() {
        let questions = parse_questions(serde_json::json!({
            "questions": [{
                "question": "Choose",
                "options": [
                    {"label": "A", "description": "First"},
                    {"label": "B", "description": "Second"}
                ],
                "recommended_options": ["B"]
            }]
        }))
        .unwrap();

        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].recommended_options, ["B"]);
        assert_eq!(questions[0].options[0].label, "B");
    }

    #[test]
    fn rejects_invalid_recommended_options_and_reserved_label() {
        for arguments in [
            serde_json::json!({
                "questions": [{
                    "question": "Choose",
                    "options": [{"label": "A", "description": "First"}],
                    "recommended_options": ["B"]
                }]
            }),
            serde_json::json!({
                "questions": [{
                    "question": "Choose",
                    "options": [{"label": "どれでもない", "description": "Reserved"}],
                    "recommended_options": []
                }]
            }),
        ] {
            assert!(parse_questions(arguments).is_err());
        }
    }

    #[test]
    fn invalid_function_tool_input_includes_the_presented_schema() {
        let error = validate_function_tool("question", serde_json::json!({}), true).unwrap_err();
        let result = error.tool_result("question");

        assert!(result.contains("Tool input schema violation for `question`."));
        assert!(result.contains("Error: invalid question arguments:"));
        assert!(result.contains("Expected schema:"));
        assert!(result.contains(r#""required":["questions"]"#));
        assert!(result.contains("Retry the tool call"));
    }

    #[test]
    fn invalid_web_search_input_is_rejected_before_provider_execution() {
        let error = validate_function_tool(
            "web_search",
            serde_json::json!({"query": "atra", "max_results": 11}),
            true,
        )
        .unwrap_err();

        assert!(
            error
                .tool_result("web_search")
                .contains("Error: property `max_results` must be an integer between 1 and 10.")
        );
    }

    #[test]
    fn invalid_custom_tool_input_includes_the_presented_grammar() {
        let error = validate_custom_tool("command", "echo missing runner").unwrap_err();
        let result = error.tool_result("command");

        assert!(result.contains("Tool input schema violation for `command`."));
        assert!(result.contains("Expected lark grammar:"));
        assert!(result.contains("start: runner_script+"));
    }

    #[test]
    fn json_command_input_is_validated_by_the_controller() {
        for (arguments, expected) in [
            (
                serde_json::json!({"command": "echo missing runner"}),
                "missing field `runner`",
            ),
            (
                serde_json::json!({
                    "runner": "sandbox",
                    "command": "echo ok",
                    "description": "unknown field"
                }),
                "unknown field `description`",
            ),
            (
                serde_json::json!({"runner": " ", "command": "echo ok"}),
                "property `runner` must not be empty",
            ),
            (
                serde_json::json!({"runner": "sandbox", "command": "\n"}),
                "property `command` must not be empty",
            ),
        ] {
            let error = validate_function_tool("command", arguments, false).unwrap_err();
            let result = error.tool_result("command");
            assert!(result.contains(expected));
            assert!(result.contains("Expected schema:"));
        }
    }

    #[test]
    fn valid_json_command_input_produces_one_runner_command() {
        let validated = validate_function_tool(
            "command",
            serde_json::json!({
                "runner": "sandbox",
                "command": "echo ok"
            }),
            false,
        )
        .unwrap();

        let ValidatedFunctionTool::Command(command) = validated else {
            panic!("expected a command");
        };
        assert_eq!(command.runner(), "sandbox");
        assert_eq!(command.command(), "echo ok");
    }

    #[test]
    fn unknown_tool_is_returned_as_a_model_error() {
        let error = validate_function_tool("unknown", serde_json::json!({}), true).unwrap_err();
        assert_eq!(
            error.tool_result("unknown"),
            "Tool call rejected for `unknown`.\n\n\
             Error: tool is not available in this turn.\n\n\
             Retry using one of the tools presented in this turn."
        );
    }
}
