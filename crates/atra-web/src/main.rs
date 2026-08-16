use std::{
    env, fs,
    os::unix::{
        fs::{OpenOptionsExt, PermissionsExt},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rustix::process::{Pid, Signal, kill_process};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    time::{Instant, sleep},
};

const DEFAULT_PORT: u16 = 2872;

#[derive(Parser)]
#[command(about = "Experimental browser Client for Atra")]
struct Args {
    #[command(subcommand)]
    command: Option<WebCommand>,
    #[arg(long, global = true)]
    port: Option<u16>,
}

#[derive(Subcommand)]
enum WebCommand {
    Serve,
    Status,
    Stop,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Daemon {
    pid: u32,
    port: u16,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("atra-web: {error:#}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    match args.command {
        Some(WebCommand::Serve) => serve(args.port.unwrap_or(DEFAULT_PORT)).await,
        Some(WebCommand::Status) => status(args.port).await,
        Some(WebCommand::Stop) => stop(args.port).await,
        None => open(args.port).await,
    }
}

async fn serve(port: u16) -> Result<()> {
    let runtime = runtime_dir()?;
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| {
            format!("cannot bind http://127.0.0.1:{port}; the port is already in use")
        })?;
    write_daemon(
        &runtime,
        &Daemon {
            pid: std::process::id(),
            port,
        },
    )?;
    println!("serving http://127.0.0.1:{port}");
    let result = atra_web::serve(listener, controller_runtime()).await;
    let _ = fs::remove_file(runtime.join("daemon.json"));
    result
}

async fn open(requested_port: Option<u16>) -> Result<()> {
    let running = running_daemon()
        .filter(|daemon| requested_port.is_none_or(|requested| requested == daemon.port));
    if let Some(daemon) = running.as_ref() {
        if healthy(daemon.port).await == Some(daemon.pid) {
            webbrowser::open(&format!("http://127.0.0.1:{}", daemon.port))
                .context("failed to open the Web Client in a browser")?;
            return Ok(());
        }
        remove_daemon_state();
    } else if let (Some(requested), Some(daemon)) = (requested_port, running_daemon()) {
        if healthy(daemon.port).await == Some(daemon.pid) {
            bail!(
                "Web daemon is already running on port {}; stop it before using port {requested}",
                daemon.port
            );
        }
        remove_daemon_state();
    }

    let port = requested_port.unwrap_or(DEFAULT_PORT);
    if healthy(port).await.is_some() {
        bail!("another Web daemon is already serving http://127.0.0.1:{port}");
    }
    spawn(port)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if healthy(port).await.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            bail!("Web daemon did not become ready within 10 seconds");
        }
        sleep(Duration::from_millis(50)).await;
    }
    webbrowser::open(&format!("http://127.0.0.1:{port}"))
        .context("failed to open the Web Client in a browser")?;
    Ok(())
}

async fn status(requested_port: Option<u16>) -> Result<()> {
    if let Some(port) = requested_port {
        if healthy(port).await.is_some() {
            println!("running at http://127.0.0.1:{port}");
        } else {
            println!("stopped");
        }
        return Ok(());
    }
    if let Some(daemon) = running_daemon() {
        if healthy(daemon.port).await == Some(daemon.pid) {
            println!("running at http://127.0.0.1:{}", daemon.port);
            return Ok(());
        }
        remove_daemon_state();
    }
    println!("stopped");
    Ok(())
}

async fn stop(requested_port: Option<u16>) -> Result<()> {
    let runtime = runtime_dir()?;
    let path = runtime.join("daemon.json");
    let daemon: Daemon =
        serde_json::from_slice(&fs::read(&path).with_context(|| "Web daemon is not running")?)
            .context("failed to decode Web daemon state")?;
    if requested_port.is_some_and(|port| daemon.port != port) {
        bail!(
            "running Web daemon uses port {}, not {}",
            daemon.port,
            requested_port.unwrap()
        );
    }
    if healthy(daemon.port).await != Some(daemon.pid) {
        let _ = fs::remove_file(path);
        bail!("Web daemon state is stale; no matching daemon is running");
    }
    let pid = Pid::from_raw(daemon.pid as i32).context("invalid Web daemon process id")?;
    kill_process(pid, Signal::TERM).context("failed to stop Web daemon")?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while healthy(daemon.port).await == Some(daemon.pid) {
        if Instant::now() >= deadline {
            bail!("Web daemon did not stop within 5 seconds");
        }
        sleep(Duration::from_millis(50)).await;
    }
    let _ = fs::remove_file(&path);
    println!("stopped");
    Ok(())
}

fn running_daemon() -> Option<Daemon> {
    let path = runtime_dir().ok()?.join("daemon.json");
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

fn remove_daemon_state() {
    if let Ok(runtime) = runtime_dir() {
        let _ = fs::remove_file(runtime.join("daemon.json"));
    }
}

fn spawn(port: u16) -> Result<()> {
    let executable = env::current_exe().context("failed to determine atra-web executable")?;
    let runtime = runtime_dir()?;
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(runtime.join("daemon.log"))
        .context("failed to open Web daemon log")?;
    let stderr = log.try_clone()?;
    let mut command = Command::new(executable);
    command
        .args(["--port", &port.to_string(), "serve"])
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(stderr);
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    command.spawn().context("failed to start Web daemon")?;
    Ok(())
}

async fn healthy(port: u16) -> Option<u32> {
    let Ok(response) = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(Duration::from_millis(300))
        .send()
        .await
    else {
        return None;
    };
    if !response.status().is_success() {
        return None;
    }
    let value = response.json::<serde_json::Value>().await.ok()?;
    (value.get("service")?.as_str()? == "atra-web")
        .then(|| {
            value
                .get("pid")?
                .as_u64()
                .and_then(|pid| pid.try_into().ok())
        })
        .flatten()
}

fn runtime_dir() -> Result<PathBuf> {
    let path = match xdg::BaseDirectories::new().get_runtime_directory() {
        Ok(path) => path.join("atra-web"),
        Err(_) => PathBuf::from(format!(
            "/tmp/atra-web-{}",
            rustix::process::getuid().as_raw()
        )),
    };
    ensure_private(&path)?;
    Ok(path)
}

fn controller_runtime() -> PathBuf {
    env::var_os("ATRA_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(
            || match xdg::BaseDirectories::new().get_runtime_directory() {
                Ok(path) => path.join("atra"),
                Err(_) => {
                    PathBuf::from(format!("/tmp/atra-{}", rustix::process::getuid().as_raw()))
                }
            },
        )
}

fn ensure_private(path: &std::path::Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    use std::os::unix::fs::MetadataExt;
    if !metadata.is_dir()
        || metadata.file_type().is_symlink()
        || metadata.uid() != rustix::process::getuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        bail!(
            "{} must be a private directory with mode 0700",
            path.display()
        );
    }
    Ok(())
}

fn write_daemon(runtime: &std::path::Path, daemon: &Daemon) -> Result<()> {
    let path = runtime.join("daemon.json");
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = runtime.join(format!(".daemon.{}.{nonce}.tmp", std::process::id()));
    let result = (|| {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&serde_json::to_vec(daemon)?)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}
