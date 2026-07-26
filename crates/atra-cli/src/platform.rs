use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use atra_store::object_digest;
use tokio::{io::AsyncWriteExt, process::Command};

pub(crate) async fn upload_runner(
    binary_path: Option<PathBuf>,
    command: Vec<String>,
) -> Result<()> {
    let binary = match binary_path {
        Some(path) => fs::read(&path)
            .with_context(|| format!("failed to read Runner binary {}", path.display()))?,
        None => installed_platform()?
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

pub(crate) fn installed_platform() -> Result<Option<atra_platform::PlatformStore>> {
    atra_platform::PlatformStore::load(data_directory()?, host_platform()?)
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
