use std::{
    env, fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_protocol::{ControllerRequest, ControllerResponse};
use clap::{Parser, Subcommand};
use rustix::process::getuid;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

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
}

#[derive(Subcommand)]
enum ControllerCommand {
    Run,
    Status,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("atra: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let command = Cli::parse().command;
    let workspace_id = workspace_id()?;
    let endpoint = controller_endpoint(&workspace_id)?;

    match command {
        Command::Controller {
            command: ControllerCommand::Run,
        } => {
            let database = controller_database(&workspace_id)?;
            atra_controller::run(&endpoint, &database).await
        }
        Command::Controller {
            command: ControllerCommand::Status,
        } => controller_status(&endpoint).await,
    }
}

fn workspace_id() -> Result<String> {
    let cwd = env::current_dir().context("failed to determine the current directory")?;
    let cwd = fs::canonicalize(&cwd)
        .with_context(|| format!("failed to resolve workspace directory {}", cwd.display()))?;
    Ok(format!("{:x}", Sha256::digest(cwd.as_os_str().as_encoded_bytes()))[..16].to_owned())
}

fn controller_endpoint(workspace_id: &str) -> Result<PathBuf> {
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

fn controller_database(workspace_id: &str) -> Result<PathBuf> {
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

async fn controller_status(endpoint: &Path) -> Result<()> {
    let mut stream = match UnixStream::connect(endpoint).await {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            println!("stopped");
            return Ok(());
        }
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to connect to controller at {}", endpoint.display())
            });
        }
    };

    let mut request = serde_json::to_vec(&ControllerRequest::Status)
        .context("failed to encode status request")?;
    request.push(b'\n');
    stream
        .write_all(&request)
        .await
        .context("failed to write status request")?;
    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .context("failed to read status response")?;
    let response: ControllerResponse =
        serde_json::from_str(&response).context("failed to decode status response")?;
    match response {
        ControllerResponse::Running => println!("running"),
    }
    Ok(())
}
