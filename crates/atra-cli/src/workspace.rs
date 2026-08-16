use std::{
    env, fs,
    io::{IsTerminal, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ApprovalPolicy, Command as StateCommand, CommandResult};
use rustix::process::getuid;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    process::Command,
    time::{Instant, sleep},
};

use crate::{
    controller_client::{client, not_running as controller_not_running},
    runner,
};

const CONFIG: &str = ".config/atra.toml";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default = "default_builtin_runners")]
    builtin_runners: bool,
    setup: Option<String>,
}

#[derive(Serialize)]
struct WorkspaceMetadata<'a> {
    workspace_id: &'a str,
    path: &'a Path,
}

fn default_builtin_runners() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            builtin_runners: true,
            setup: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum ControllerStart {
    Started,
    AlreadyRunning,
}

pub(crate) fn root() -> Result<PathBuf> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    fs::canonicalize(&cwd)
        .with_context(|| format!("failed to resolve workspace directory {}", cwd.display()))
}

pub(crate) fn id(workspace: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(workspace.as_os_str().as_encoded_bytes())
    )[..16]
        .to_owned()
}

fn load_config(workspace: &Path) -> Result<Config> {
    let path = workspace.join(CONFIG);
    let config = match fs::read_to_string(&path) {
        Ok(config) => config,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read workspace config {}", path.display()));
        }
    };
    toml::from_str(&config)
        .with_context(|| format!("failed to parse workspace config {}", path.display()))
}

pub(crate) async fn start(workspace: &Path) -> Result<()> {
    let config = load_config(workspace)?;
    let workspace_id = id(workspace);
    let endpoint = endpoint(&workspace_id)?;
    let database = database(&workspace_id)?;
    start_controller(workspace, &endpoint, &database).await?;
    if config.builtin_runners {
        launch_builtin_runners(workspace, &endpoint).await?;
    }
    if let Some(setup) = &config.setup {
        run_setup(workspace, &endpoint, setup).await?;
    }
    println!("workspace started");
    Ok(())
}

async fn launch_builtin_runners(workspace: &Path, endpoint: &Path) -> Result<()> {
    launch_host(endpoint).await?;
    launch_sandbox(workspace, endpoint).await?;
    Ok(())
}

async fn launch_host(endpoint: &Path) -> Result<()> {
    runner::launch(
        endpoint,
        runner::RunnerLaunch {
            name: "host".to_owned(),
            description: "Run commands directly in the workspace host environment.".to_owned(),
            approval: ApprovalPolicy::Ask,
            command: runner::runner_command(Vec::new())?,
        },
    )
    .await
}

async fn launch_sandbox(workspace: &Path, endpoint: &Path) -> Result<()> {
    let executable = env::current_exe().context("failed to determine the atra executable path")?;
    let executable = executable
        .into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("atra executable path is not valid UTF-8"))?;
    let workspace = workspace
        .as_os_str()
        .to_str()
        .context("workspace path is not valid UTF-8")?;
    runner::launch(
        endpoint,
        runner::RunnerLaunch {
            name: "sandbox".to_owned(),
            description: "Sandboxed workspace environment. Only the workspace and the \
                          sandbox HOME are writable by default; the host filesystem is \
                          visible read-only and the network is shared."
                .to_owned(),
            approval: ApprovalPolicy::Allow,
            command: vec![
                executable,
                "runner".to_owned(),
                "sandbox".to_owned(),
                "--preset".to_owned(),
                "standard".to_owned(),
                "--workspace".to_owned(),
                workspace.to_owned(),
            ],
        },
    )
    .await
}

async fn run_setup(workspace: &Path, endpoint: &Path, setup: &str) -> Result<()> {
    let atra_binary = env::current_exe().context("failed to determine the atra executable path")?;
    let status = Command::new("bash")
        .args(["-c", setup])
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
    Ok(())
}

pub(crate) async fn clean(workspace: &Path, endpoint: &Path, force: bool) -> Result<()> {
    if controller_is_running(endpoint).await? {
        bail!("controller is running; stop it before cleaning the workspace");
    }
    let home = sandbox_home_path(workspace)?;
    if !home.exists() {
        return Ok(());
    }
    for directory in sandbox_directories(workspace)? {
        check_private_directory(&directory)?;
    }
    if !force {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            bail!("workspace clean requires --force when not running interactively");
        }
        print!("Remove {}? [y/N] ", home.display());
        std::io::stdout()
            .flush()
            .context("failed to display workspace clean prompt")?;
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .context("failed to read workspace clean confirmation")?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            return Ok(());
        }
    }
    fs::remove_dir_all(&home)
        .with_context(|| format!("failed to remove sandbox home {}", home.display()))?;
    println!("removed {}", home.display());
    Ok(())
}

pub(crate) async fn prepare_tui(workspace: &Path, endpoint: &Path) -> Result<bool> {
    if controller_is_running(endpoint).await? {
        return Ok(true);
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
    start(workspace).await?;
    Ok(true)
}

pub(crate) fn endpoint(workspace_id: &str) -> Result<PathBuf> {
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

pub(crate) fn database(workspace_id: &str) -> Result<PathBuf> {
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

pub(crate) fn sandbox_home(workspace: &Path) -> Result<PathBuf> {
    let home = sandbox_home_path(workspace)?;
    let state_home = xdg::BaseDirectories::new()
        .get_state_home()
        .context("cannot determine the XDG state directory")?;
    fs::create_dir_all(&state_home)
        .with_context(|| format!("failed to create state directory {}", state_home.display()))?;
    for directory in sandbox_directories(workspace)? {
        ensure_private_directory(&directory)?;
    }
    Ok(home)
}

fn sandbox_home_path(workspace: &Path) -> Result<PathBuf> {
    Ok(sandbox_directories(workspace)?
        .pop()
        .expect("sandbox directories should not be empty"))
}

fn sandbox_directories(workspace: &Path) -> Result<Vec<PathBuf>> {
    let workspace_id = id(workspace);
    let state_home = xdg::BaseDirectories::new()
        .get_state_home()
        .context("cannot determine the XDG state directory")?;
    let atra = state_home.join("atra");
    let workspaces = atra.join("workspaces");
    let workspace = workspaces.join(workspace_id);
    let sandbox = workspace.join("sandbox");
    let home = sandbox.join("home");
    Ok(vec![atra, workspaces, workspace, sandbox, home])
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
    check_private_directory(path)
}

fn check_private_directory(path: &Path) -> Result<()> {
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

pub(crate) async fn start_controller(
    workspace: &Path,
    endpoint: &Path,
    database: &Path,
) -> Result<ControllerStart> {
    if controller_is_running(endpoint).await? {
        write_workspace_metadata(endpoint, &id(workspace), workspace)?;
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
    let mut command = Command::new(executable);
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
            write_workspace_metadata(endpoint, &id(workspace), workspace)?;
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

fn write_workspace_metadata(endpoint: &Path, workspace_id: &str, workspace: &Path) -> Result<()> {
    let directory = endpoint
        .parent()
        .context("controller endpoint has no runtime directory")?;
    // Custom controller endpoints may live in caller-owned directories that are
    // intentionally not private. They remain usable by the CLI, but are not
    // published for automatic Web daemon discovery.
    if check_private_directory(directory).is_err() {
        return Ok(());
    }
    let path = directory.join("workspace.json");
    let temporary = directory.join(format!(".workspace.json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec(&WorkspaceMetadata {
        workspace_id,
        path: workspace,
    })
    .context("failed to encode Workspace runtime metadata")?;
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
            .with_context(|| {
                format!(
                    "failed to create Workspace runtime metadata {}",
                    temporary.display()
                )
            })?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, &path).with_context(|| {
            format!(
                "failed to publish Workspace runtime metadata {}",
                path.display()
            )
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) async fn stop_controller(endpoint: &Path) -> Result<()> {
    match send_shutdown(endpoint).await {
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

pub(crate) async fn controller_status(endpoint: &Path) -> Result<()> {
    match client(endpoint).subscribe_controller().await {
        Ok(_) => println!("running"),
        Err(error) if controller_not_running(&error) => println!("stopped"),
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn send_shutdown(endpoint: &Path) -> Result<()> {
    match client(endpoint).command(StateCommand::Shutdown).await? {
        CommandResult::Accepted => Ok(()),
        result => bail!("unexpected shutdown result: {result:?}"),
    }
}

async fn controller_is_running(endpoint: &Path) -> Result<bool> {
    match client(endpoint).subscribe_controller().await {
        Ok(_) => Ok(true),
        Err(error) if controller_not_running(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_config_without_file_uses_defaults() {
        let directory = tempfile::tempdir().unwrap();
        let config = load_config(directory.path()).unwrap();
        assert!(config.builtin_runners);
        assert_eq!(config.setup, None);
    }

    #[test]
    fn config_combinations() {
        let cases = [
            ("", true, None),
            ("builtin_runners = true", true, None),
            ("builtin_runners = false", false, None),
            ("setup = \"bash setup.bash\"", true, Some("bash setup.bash")),
            ("builtin_runners = true\nsetup = \"x\"", true, Some("x")),
            ("builtin_runners = false\nsetup = \"x\"", false, Some("x")),
        ];
        for (input, builtin, setup) in cases {
            let config: Config = toml::from_str(input).unwrap();
            assert_eq!(config.builtin_runners, builtin, "input: {input:?}");
            assert_eq!(config.setup.as_deref(), setup, "input: {input:?}");
        }
    }

    #[test]
    fn config_rejects_unknown_field() {
        assert!(toml::from_str::<Config>("unknown = 1").is_err());
    }

    #[test]
    fn config_rejects_malformed_toml() {
        assert!(toml::from_str::<Config>("builtin_runners = ").is_err());
    }

    #[test]
    fn load_config_rejects_unreadable_file() {
        let directory = tempfile::tempdir().unwrap();
        fs::create_dir_all(directory.path().join(".config")).unwrap();
        fs::write(directory.path().join(CONFIG), "builtin_runners = ").unwrap();
        assert!(load_config(directory.path()).is_err());
    }

    #[test]
    fn workspace_metadata_is_private_and_strict() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let runtime = directory.path().join("runtime");
        fs::create_dir(&workspace).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();

        write_workspace_metadata(&runtime.join("controller.sock"), "abc", &workspace).unwrap();

        let path = runtime.join("workspace.json");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(value["workspace_id"], "abc");
        assert_eq!(value["path"], workspace.to_str().unwrap());
    }
}
