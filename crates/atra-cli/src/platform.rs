use std::{
    env, fs,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, process::Command};

pub(crate) async fn upload_runner(
    binary_path: Option<PathBuf>,
    command: Vec<String>,
) -> Result<()> {
    let binary = match binary_path {
        Some(path) => fs::read(&path)
            .with_context(|| format!("failed to read Runner binary {}", path.display()))?,
        None => {
            let bundle_path = current_bundle()?;
            atra_platform::PlatformBundle::load(&bundle_path)?
                .runner()
                .decompress()
                .with_context(|| {
                    format!(
                        "failed to extract Runner from platform bundle {}",
                        bundle_path.display()
                    )
                })?
        }
    };
    let digest = format!("{:x}", Sha256::digest(&binary));
    let remote_path = format!("/tmp/atra-runner/{digest}/atra-runner");
    let script = format!(
        "path='{remote_path}'; if [ -x \"$path\" ]; then cat >/dev/null; else \
         mkdir -p \"${{path%/*}}\" && temporary=\"$path.tmp.$$\" && cat >\"$temporary\" && \
         chmod 755 \"$temporary\" && mv \"$temporary\" \"$path\"; fi; printf '%s\\n' \"$path\""
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
    if reported_path.trim() != remote_path {
        bail!("Runner upload returned an unexpected path: {reported_path:?}");
    }
    println!("{remote_path}");
    Ok(())
}

pub(crate) fn install(source: &Path) -> Result<()> {
    let bundle = atra_platform::PlatformBundle::load(source)?;
    bundle.runner().decompress()?;
    for name in bundle.tool_names() {
        bundle
            .tool(&name)
            .expect("listed platform tool should exist")
            .decompress()
            .with_context(|| format!("failed to verify bundled tool {name}"))?;
    }

    let bytes = fs::read(source)
        .with_context(|| format!("failed to read platform bundle {}", source.display()))?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let platform_directory = data_directory()?.join(bundle.platform());
    let bundles_directory = platform_directory.join("bundles");
    fs::create_dir_all(&bundles_directory).with_context(|| {
        format!(
            "failed to create platform bundle directory {}",
            bundles_directory.display()
        )
    })?;
    let installed = bundles_directory.join(format!("{digest}.zip"));
    if !installed.exists() {
        let temporary = bundles_directory.join(format!(".{digest}.tmp-{}", std::process::id()));
        fs::write(&temporary, bytes).with_context(|| {
            format!(
                "failed to write platform bundle temporary file {}",
                temporary.display()
            )
        })?;
        fs::rename(&temporary, &installed).with_context(|| {
            format!("failed to install platform bundle {}", installed.display())
        })?;
    }

    let current = platform_directory.join("current");
    let temporary = platform_directory.join(format!(".current.tmp-{}", std::process::id()));
    fs::write(&temporary, format!("{digest}\n"))
        .with_context(|| format!("failed to write current bundle {}", temporary.display()))?;
    fs::rename(&temporary, &current)
        .with_context(|| format!("failed to select current bundle {}", current.display()))?;
    println!("{}", installed.display());
    Ok(())
}

fn current_bundle() -> Result<PathBuf> {
    if let Some(path) = env::var_os("ATRA_PLATFORM_BUNDLE") {
        return Ok(PathBuf::from(path));
    }
    let platform_directory = data_directory()?.join(host_platform()?);
    let current = platform_directory.join("current");
    let digest = fs::read_to_string(&current).with_context(|| {
        format!(
            "failed to read current platform bundle {}",
            current.display()
        )
    })?;
    let digest = digest.trim();
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("current platform bundle contains an invalid digest");
    }
    Ok(platform_directory
        .join("bundles")
        .join(format!("{digest}.zip")))
}

fn data_directory() -> Result<PathBuf> {
    Ok(xdg::BaseDirectories::new()
        .get_data_home()
        .context("cannot determine the XDG data directory")?
        .join("atra/platforms"))
}

fn host_platform() -> Result<&'static str> {
    match env::consts::ARCH {
        "x86_64" => Ok("x86_64-linux-musl"),
        "aarch64" => Ok("aarch64-linux-musl"),
        architecture => bail!("unsupported host architecture {architecture}"),
    }
}
