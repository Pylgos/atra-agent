use std::{env, path::Path};

use anyhow::{Context, Result, bail};
use atra_protocol::{ApprovalPolicy, Command as StateCommand, CommandResult, RunnerLifecycle};

use crate::controller_client::client;

pub(crate) struct RunnerLaunch {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) approval: ApprovalPolicy,
    pub(crate) command: Vec<String>,
}

pub(crate) async fn launch(endpoint: &Path, launch: RunnerLaunch) -> Result<()> {
    let mut subscription = client(endpoint).subscribe_controller().await?;
    match client(endpoint)
        .command(StateCommand::RunnerLaunch {
            name: launch.name.clone(),
            description: launch.description,
            approval: launch.approval,
            command: launch.command,
        })
        .await?
    {
        CommandResult::Accepted => {}
        result => bail!("unexpected command result: {result:?}"),
    }
    loop {
        subscription.receive().await?;
        let lifecycle = subscription
            .state()
            .runners()
            .iter()
            .find(|runner| runner.runner().name == launch.name)
            .with_context(|| format!("Runner {} is not available", launch.name))?
            .lifecycle();
        match lifecycle {
            RunnerLifecycle::Launching => {}
            RunnerLifecycle::Running => break,
            RunnerLifecycle::Failed { message } => bail!(message.clone()),
        }
    }
    Ok(())
}

pub(crate) fn runner_command(command: Vec<String>) -> Result<Vec<String>> {
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
