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
    let endpoint = controller_endpoint()?;

    match command {
        Command::Controller {
            command: ControllerCommand::Run,
        } => atra_controller::run(&endpoint).await,
        Command::Controller {
            command: ControllerCommand::Status,
        } => controller_status(&endpoint).await,
    }
}

fn controller_endpoint() -> Result<PathBuf, AppError> {
    if let Some(endpoint) = env::var_os("ATRA_CONTROLLER_ENDPOINT") {
        return Ok(PathBuf::from(endpoint));
    }

    let cwd = fs::canonicalize(env::current_dir()?)?;
    let workspace_id = format!("{:x}", Sha256::digest(cwd.as_os_str().as_encoded_bytes()));
    let runtime_dir = match xdg::BaseDirectories::new().get_runtime_directory() {
        Ok(path) => path.join("atra"),
        Err(_) => PathBuf::from(format!("/tmp/atra-{}", getuid().as_raw())),
    };

    ensure_private_directory(&runtime_dir)?;
    let workspace_dir = runtime_dir.join(&workspace_id[..16]);
    ensure_private_directory(&workspace_dir)?;
    Ok(workspace_dir.join("controller.sock"))
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
