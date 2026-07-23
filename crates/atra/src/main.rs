use std::{
    env,
    error::Error,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
};

use atra_protocol::{ControllerRequest, ControllerResponse};
use clap::{Parser, Subcommand};
use rustix::process::getuid;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
};

type AppError = Box<dyn Error + Send + Sync>;

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
        eprintln!("atra: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), AppError> {
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

fn workspace_id() -> Result<String, AppError> {
    let cwd = fs::canonicalize(env::current_dir()?)?;
    Ok(format!("{:x}", Sha256::digest(cwd.as_os_str().as_encoded_bytes()))[..16].to_owned())
}

fn controller_endpoint(workspace_id: &str) -> Result<PathBuf, AppError> {
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

fn controller_database(workspace_id: &str) -> Result<PathBuf, AppError> {
    if let Some(database) = env::var_os("ATRA_CONTROLLER_STATE") {
        return Ok(PathBuf::from(database));
    }

    let state_home = xdg::BaseDirectories::new()
        .get_state_home()
        .ok_or("cannot determine the XDG state directory")?;
    fs::create_dir_all(&state_home)?;
    let atra_dir = state_home.join("atra");
    ensure_private_directory(&atra_dir)?;
    let workspace_dir = atra_dir.join(workspace_id);
    ensure_private_directory(&workspace_dir)?;
    Ok(workspace_dir.join("controller.sqlite3"))
}

fn ensure_private_directory(path: &Path) -> Result<(), AppError> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }

    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(format!(
            "{} must be a directory owned by the current user with mode 0700",
            path.display()
        )
        .into());
    }
    Ok(())
}

async fn controller_status(endpoint: &Path) -> Result<(), AppError> {
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
        Err(error) => return Err(error.into()),
    };

    let mut request = serde_json::to_vec(&ControllerRequest::Status)?;
    request.push(b'\n');
    stream.write_all(&request).await?;
    let mut response = String::new();
    BufReader::new(stream).read_line(&mut response).await?;
    let response: ControllerResponse = serde_json::from_str(&response)?;
    match response {
        ControllerResponse::Running => println!("running"),
    }
    Ok(())
}
