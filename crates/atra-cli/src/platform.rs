use std::{
    env,
    ffi::OsStr,
    fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use atra_store::object_digest;
use tokio::{io::AsyncWriteExt, process::Command};

const BUILD_COMMIT: Option<&str> = option_env!("ATRA_BUILD_COMMIT");
const RELEASES_URL: &str = "https://github.com/Pylgos/atra-agent/releases/download";

pub(crate) async fn download() -> Result<()> {
    let commit = BUILD_COMMIT
        .context("platform download is only available in Atra binaries built by the official CI")?;
    let asset = format!("atra-platform-{}.zip", host_platform()?);
    let url = format!("{RELEASES_URL}/build-{commit}/{asset}");
    let mut response = reqwest::Client::new()
        .get(&url)
        .header(reqwest::header::USER_AGENT, "atra")
        .send()
        .await
        .with_context(|| format!("failed to download platform bundle from {url}"))?
        .error_for_status()
        .with_context(|| format!("failed to download platform bundle from {url}"))?;
    let temporary =
        tempfile::NamedTempFile::new().context("failed to create temporary platform bundle")?;
    let mut destination = tokio::fs::File::from_std(
        temporary
            .reopen()
            .context("failed to open temporary platform bundle")?,
    );
    while let Some(chunk) = response
        .chunk()
        .await
        .context("failed to read platform bundle download")?
    {
        destination
            .write_all(&chunk)
            .await
            .context("failed to write temporary platform bundle")?;
    }
    destination
        .flush()
        .await
        .context("failed to flush temporary platform bundle")?;
    install(temporary.path())
}

pub(crate) async fn upload_runner(
    binary_path: Option<PathBuf>,
    command: Vec<String>,
) -> Result<()> {
    let binary = match binary_path {
        Some(path) => fs::read(&path)
            .with_context(|| format!("failed to read Runner binary {}", path.display()))?,
        None => current_platform()?
            .context("no default platform is installed")?
            .runner()?,
    };
    let digest = object_digest(&binary, true);
    let script = format!(
        "root=\"${{TMPDIR:-/tmp}}/atra-$(id -u)\"; \
         path=\"$root/objects/{digest}\"; \
         if [ -x \"$path\" ]; then cat >/dev/null; else \
         umask 077; mkdir -p \"$root/objects\"; temporary=\"$path.tmp.$$\"; \
         cat >\"$temporary\"; chmod 555 \"$temporary\"; \
         if ! ln \"$temporary\" \"$path\" 2>/dev/null; then test -x \"$path\"; fi; \
         rm -f \"$temporary\"; fi; printf '%s\\n' \"$path\""
    );

    let mut child = Command::new(&command[0])
        .args(&command[1..])
        .args(["-c", &script])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to start upload command {}", command[0]))?;
    child
        .stdin
        .take()
        .context("upload command stdin was not available")?
        .write_all(&binary)
        .await
        .context("failed to stream Runner binary")?;
    let output = child
        .wait_with_output()
        .await
        .context("failed to wait for Runner upload")?;
    if !output.status.success() {
        bail!("Runner upload command exited with {}", output.status);
    }
    let reported_path =
        String::from_utf8(output.stdout).context("Runner upload path was not valid UTF-8")?;
    let reported_path = reported_path.trim();
    if !Path::new(reported_path).is_absolute()
        || !reported_path.ends_with(&format!("/objects/{digest}"))
    {
        bail!("Runner upload returned an unexpected path: {reported_path:?}");
    }
    println!("{reported_path}");
    Ok(())
}

pub(crate) fn install(source: &Path) -> Result<()> {
    let bundle = atra_platform::PlatformBundle::load(source)?;
    let installed = bundle.install(&data_directory()?)?;
    println!("{}", installed.display());
    Ok(())
}

pub(crate) async fn exec(tool: &OsStr, args: &[std::ffi::OsString]) -> Result<()> {
    validate_tool_name(tool)?;
    let platform = current_platform()?.context("no default platform is installed")?;
    let path = platform.tool_path(tool)?;
    let status = Command::new(&path)
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("failed to start platform tool {}", path.display()))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

fn validate_tool_name(name: &OsStr) -> Result<()> {
    let name = name
        .to_str()
        .with_context(|| format!("platform tool name {name:?} is not valid UTF-8"))?;
    if name.is_empty() || matches!(name, "." | "..") || name.contains('/') || name.contains('\\') {
        bail!("invalid platform tool name {name:?}");
    }
    Ok(())
}

pub(crate) fn current_platform() -> Result<Option<atra_platform::PlatformStore>> {
    let platform = host_platform()?;
    let executable = env::current_exe().context("failed to determine the atra executable path")?;
    if let Some(root) = executable
        .parent()
        .and_then(Path::parent)
        .map(|root| root.join("share/atra"))
        .filter(|root| root.is_dir())
        && let Some(platform) = atra_platform::PlatformStore::load(root, platform)?
    {
        return Ok(Some(platform));
    }
    atra_platform::PlatformStore::load(data_directory()?, platform)
}

fn data_directory() -> Result<PathBuf> {
    Ok(xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")?
        .join("atra"))
}

pub(crate) fn host_platform() -> Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("x86_64-linux-static"),
        "aarch64" => Ok("aarch64-linux-static"),
        architecture => bail!("unsupported host architecture {architecture}"),
    }
}
