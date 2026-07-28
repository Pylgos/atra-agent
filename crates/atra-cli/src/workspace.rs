use std::{
    env, fs,
    io::{IsTerminal, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use atra_protocol::ControllerRequest;
use rustix::process::getuid;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::AsyncWriteExt,
    net::UnixStream,
    process::Command,
    time::{Instant, sleep},
};

use crate::controller_client::{client, not_running as controller_not_running};

const CONFIG: &str = ".config/atra.toml";
const SETUP: &str = ".config/atra-setup.bash";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    setup: String,
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

pub(crate) fn init(workspace: &Path) -> Result<()> {
    let config_path = workspace.join(CONFIG);
    let setup_path = workspace.join(SETUP);
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
    fs::write(&config_path, format!("setup = \"bash {SETUP}\"\n"))
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

fn load_config(workspace: &Path) -> Result<Config> {
    let path = workspace.join(CONFIG);
    let config = fs::read_to_string(&path)
        .with_context(|| format!("failed to read workspace config {}", path.display()))?;
    toml::from_str(&config)
        .with_context(|| format!("failed to parse workspace config {}", path.display()))
}

pub(crate) async fn start(workspace: &Path, endpoint: &Path, workspace_id: &str) -> Result<()> {
    let config = load_config(workspace)?;
    let database = database(workspace_id)?;
    start_controller(workspace, endpoint, &database).await?;

    let atra_binary = env::current_exe().context("failed to determine the atra executable path")?;
    let status = Command::new("bash")
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

pub(crate) async fn prepare_tui(
    workspace: &Path,
    endpoint: &Path,
    workspace_id: &str,
) -> Result<bool> {
    if controller_is_running(endpoint).await? {
        return Ok(true);
    }
    if !workspace.join(CONFIG).is_file() {
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
    start(workspace, endpoint, workspace_id).await?;
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

pub(crate) async fn start_controller(
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
    match client(endpoint).status().await {
        Ok(()) => println!("running"),
        Err(error) if controller_not_running(&error) => println!("stopped"),
        Err(error) => return Err(error),
    }
    Ok(())
}

async fn send_shutdown(endpoint: &Path) -> Result<()> {
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
    match client(endpoint).status().await {
        Ok(()) => Ok(true),
        Err(error) if controller_not_running(&error) => Ok(false),
        Err(error) => Err(error),
    }
}
