use std::{error::Error, fmt};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct RunnerCommand {
    runner: String,
    command: String,
}

impl RunnerCommand {
    pub fn from_parts(runner: String, command: String) -> Self {
        Self { runner, command }
    }

    pub fn runner(&self) -> &str {
        &self.runner
    }

    pub fn command(&self) -> &str {
        &self.command
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandParseError(&'static str);

impl fmt::Display for CommandParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for CommandParseError {}

pub fn parse_command_input(input: &str) -> Result<Vec<RunnerCommand>, CommandParseError> {
    let lines = input.lines().collect::<Vec<_>>();
    let mut runner = None;
    let mut command_start = 0;
    let mut operations = Vec::new();

    for (index, line) in lines.iter().enumerate() {
        if let Some(name) = line.strip_prefix("*** Runner ") {
            if let Some(runner) = runner.replace(name.to_owned()) {
                if command_start == index {
                    return Err(CommandParseError("runner group must contain a command"));
                }
                operations.push(RunnerCommand {
                    runner,
                    command: lines[command_start..index].join("\n"),
                });
            }
            if name.is_empty() {
                return Err(CommandParseError("runner name cannot be empty"));
            }
            command_start = index + 1;
        } else if runner.is_none() {
            return Err(CommandParseError(
                "command input must start with '*** Runner <runner>'",
            ));
        }
    }

    let runner = runner.ok_or(CommandParseError(
        "command input must contain at least one runner group",
    ))?;
    if command_start == lines.len() {
        return Err(CommandParseError("runner group must contain a command"));
    }
    operations.push(RunnerCommand {
        runner,
        command: lines[command_start..].join("\n"),
    });
    Ok(operations)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_multiple_runner_groups_without_losing_multiline_commands() {
        let parsed =
            parse_command_input("*** Runner sandbox\nset -e\necho one\n*** Runner host\npwd")
                .unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].runner(), "sandbox");
        assert_eq!(parsed[0].command(), "set -e\necho one");
        assert_eq!(parsed[1].runner(), "host");
        assert_eq!(parsed[1].command(), "pwd");
    }

    #[test]
    fn rejects_text_before_the_first_runner() {
        assert!(parse_command_input("echo nope").is_err());
    }
}
