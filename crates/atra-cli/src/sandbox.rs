use std::{
    env,
    ffi::OsString,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Stdio,
};

use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use rustix::process::{getgid, getuid};
use tokio::process::Command;

use crate::{platform, workspace};

const SANDBOX_HOME: &str = "/home/atra";
const RUNNER_PATH: &str = "/run/atra-runner";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub(crate) enum SandboxPreset {
    Standard,
    Relaxed,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct SandboxOptions {
    #[arg(long, value_enum, default_value = "standard")]
    pub(crate) preset: SandboxPreset,
    #[arg(long)]
    pub(crate) workspace: Option<PathBuf>,
    #[arg(long = "mount-ro")]
    pub(crate) mount_ro: Vec<PathBuf>,
    #[arg(long = "mount-rw")]
    pub(crate) mount_rw: Vec<PathBuf>,
    #[arg(long)]
    pub(crate) bwrap: Option<PathBuf>,
    #[arg(long = "runner-binary")]
    pub(crate) runner_binary: Option<PathBuf>,
    #[arg(long = "bwrap-arg", allow_hyphen_values = true)]
    pub(crate) bwrap_args: Vec<OsString>,
}

struct SandboxContext {
    workspace: PathBuf,
    sandbox_home: PathBuf,
    runner_binary: PathBuf,
    bwrap: PathBuf,
    hidden_host_home: Option<PathBuf>,
    uid: u32,
    gid: u32,
}

struct SandboxPlan {
    bwrap: PathBuf,
    args: Vec<OsString>,
}

pub(crate) async fn execute(options: SandboxOptions) -> Result<()> {
    if !cfg!(target_os = "linux") {
        bail!("the sandbox runner is only supported on Linux");
    }
    let bwrap = resolve_bwrap(&options)?;
    let workspace = resolve_workspace(&options)?;
    let sandbox_home = workspace::sandbox_home(&workspace)?;
    let runner_binary = resolve_runner_binary(&options)?;
    let hidden_host_home = match options.preset {
        SandboxPreset::Standard => Some(resolve_host_home(env::var_os("HOME"))?),
        SandboxPreset::Relaxed => None,
    };
    let context = SandboxContext {
        workspace,
        sandbox_home,
        runner_binary,
        bwrap,
        hidden_host_home,
        uid: getuid().as_raw(),
        gid: getgid().as_raw(),
    };
    let plan = build_plan(options, context)?;
    run(plan).await
}

fn resolve_workspace(options: &SandboxOptions) -> Result<PathBuf> {
    let workspace = match &options.workspace {
        Some(path) => path.clone(),
        None => env::current_dir().context("failed to determine the current directory")?,
    };
    fs::canonicalize(&workspace).with_context(|| {
        format!(
            "failed to resolve workspace directory {}",
            workspace.display()
        )
    })
}

fn resolve_host_home(home: Option<OsString>) -> Result<PathBuf> {
    let home = PathBuf::from(
        home.context("HOME is not set; the standard sandbox requires a host HOME directory")?,
    );
    let home = fs::canonicalize(&home)
        .with_context(|| format!("failed to resolve host HOME {}", home.display()))?;
    if !home.is_dir() {
        bail!("host HOME {} is not a directory", home.display());
    }
    Ok(home)
}

fn resolve_bwrap(options: &SandboxOptions) -> Result<PathBuf> {
    if let Some(path) = &options.bwrap {
        return validate_executable(path);
    }
    if let Some(path) = find_in_path("bwrap") {
        return Ok(path);
    }
    let platform = platform::current_platform()?.context("no default platform is installed")?;
    platform.tool_path(std::ffi::OsStr::new("bwrap"))
}

fn resolve_runner_binary(options: &SandboxOptions) -> Result<PathBuf> {
    match &options.runner_binary {
        Some(path) => validate_executable(path),
        None => platform::current_platform()?
            .context("no default platform is installed")?
            .runner_path(),
    }
}

fn validate_executable(path: &Path) -> Result<PathBuf> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("failed to resolve executable {}", path.display()))?;
    let metadata = fs::metadata(&path)
        .with_context(|| format!("failed to inspect executable {}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("{} is not an executable file", path.display());
    }
    Ok(path)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for directory in env::split_paths(&path) {
        let candidate = directory.join(name);
        if candidate.is_file() && is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

fn canonicalize_mount(path: &Path) -> Result<PathBuf> {
    fs::canonicalize(path)
        .with_context(|| format!("failed to resolve mount path {}", path.display()))
}

fn build_plan(options: SandboxOptions, context: SandboxContext) -> Result<SandboxPlan> {
    let mut args = Vec::new();

    // Namespaces: user, pid, ipc, uts, cgroup, and mount (always created by
    // bwrap). The network namespace is intentionally shared with the host.
    for flag in [
        "--unshare-user",
        "--unshare-pid",
        "--unshare-ipc",
        "--unshare-uts",
        "--unshare-cgroup",
    ] {
        args.push(OsString::from(flag));
    }

    // Identity inside the user namespace matches the launching user.
    args.push(OsString::from("--uid"));
    args.push(OsString::from(context.uid.to_string()));
    args.push(OsString::from("--gid"));
    args.push(OsString::from(context.gid.to_string()));

    // Terminate the sandbox when the transport process dies.
    args.push(OsString::from("--die-with-parent"));

    // Base filesystem.
    args.push(OsString::from("--ro-bind"));
    args.push(OsString::from("/"));
    args.push(OsString::from("/"));
    args.push(OsString::from("--proc"));
    args.push(OsString::from("/proc"));
    args.push(OsString::from("--dev"));
    args.push(OsString::from("/dev"));
    for directory in ["/tmp", "/run", "/var/tmp"] {
        args.push(OsString::from("--tmpfs"));
        args.push(OsString::from(directory));
    }
    args.push(OsString::from("--ro-bind"));
    args.push(OsString::from("/sys"));
    args.push(OsString::from("/sys"));

    // Preset visibility of the host HOME.
    if let Some(home) = context.hidden_host_home {
        args.push(OsString::from("--tmpfs"));
        args.push(home.into_os_string());
    }

    // Persistent sandbox HOME.
    args.push(OsString::from("--bind"));
    args.push(context.sandbox_home.into_os_string());
    args.push(OsString::from(SANDBOX_HOME));

    // Canonical workspace at its original absolute path.
    args.push(OsString::from("--bind"));
    args.push(context.workspace.clone().into_os_string());
    args.push(context.workspace.clone().into_os_string());

    // Runner binary at a fixed private path.
    args.push(OsString::from("--ro-bind"));
    args.push(context.runner_binary.into_os_string());
    args.push(OsString::from(RUNNER_PATH));

    // Explicit mounts, in the order given.
    for path in &options.mount_ro {
        let path = canonicalize_mount(path)?;
        args.push(OsString::from("--ro-bind"));
        args.push(path.clone().into_os_string());
        args.push(path.into_os_string());
    }
    for path in &options.mount_rw {
        let path = canonicalize_mount(path)?;
        args.push(OsString::from("--bind"));
        args.push(path.clone().into_os_string());
        args.push(path.into_os_string());
    }

    // Environment overrides.
    args.push(OsString::from("--setenv"));
    args.push(OsString::from("HOME"));
    args.push(OsString::from(SANDBOX_HOME));
    for variable in ["TMPDIR", "TMP", "TEMP"] {
        args.push(OsString::from("--setenv"));
        args.push(OsString::from(variable));
        args.push(OsString::from("/tmp"));
    }

    // Working directory.
    args.push(OsString::from("--chdir"));
    args.push(context.workspace.into_os_string());

    // Raw arguments, then the standalone Runner command.
    args.extend(options.bwrap_args);
    args.push(OsString::from("--"));
    args.push(OsString::from(RUNNER_PATH));
    args.push(OsString::from("--stdio"));

    Ok(SandboxPlan {
        bwrap: context.bwrap,
        args,
    })
}

async fn run(plan: SandboxPlan) -> Result<()> {
    let status = Command::new(&plan.bwrap)
        .args(&plan.args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .with_context(|| format!("failed to start Bubblewrap {}", plan.bwrap.display()))?;
    if !status.success() {
        bail!("Bubblewrap exited with {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn context() -> SandboxContext {
        SandboxContext {
            workspace: PathBuf::from("/ws"),
            sandbox_home: PathBuf::from("/state/ws/sandbox/home"),
            runner_binary: PathBuf::from("/platform/runner"),
            bwrap: PathBuf::from("/usr/bin/bwrap"),
            hidden_host_home: Some(PathBuf::from("/home/user")),
            uid: 1000,
            gid: 1000,
        }
    }

    fn options() -> SandboxOptions {
        SandboxOptions {
            preset: SandboxPreset::Standard,
            workspace: None,
            mount_ro: Vec::new(),
            mount_rw: Vec::new(),
            bwrap: None,
            runner_binary: None,
            bwrap_args: Vec::new(),
        }
    }

    fn args(plan: &SandboxPlan) -> Vec<String> {
        plan.args
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn position(arguments: &[String], needle: &str) -> usize {
        arguments
            .iter()
            .position(|argument| argument == needle)
            .unwrap_or_else(|| panic!("{needle:?} not found in {arguments:?}"))
    }

    #[test]
    fn standard_hides_host_home() {
        let plan = build_plan(options(), context()).unwrap();
        let arguments = args(&plan);
        assert!(arguments.contains(&"--tmpfs".to_owned()));
        assert!(arguments.contains(&"/home/user".to_owned()));
    }

    #[test]
    fn host_home_must_be_set() {
        let error = resolve_host_home(None).unwrap_err();
        assert!(error.to_string().contains("HOME is not set"));
    }

    #[test]
    fn host_home_is_canonicalized() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        let home_link = directory.path().join("home-link");
        fs::create_dir(&home).unwrap();
        symlink(&home, &home_link).unwrap();

        assert_eq!(
            resolve_host_home(Some(home_link.into_os_string())).unwrap(),
            home
        );
    }

    #[test]
    fn relaxed_keeps_host_home_visible() {
        let mut options = options();
        options.preset = SandboxPreset::Relaxed;
        let mut context = context();
        context.hidden_host_home = None;
        let plan = build_plan(options, context).unwrap();
        let arguments = args(&plan);
        assert!(!arguments.contains(&"/home/user".to_owned()));
    }

    #[test]
    fn workspace_mount_comes_after_home_hiding() {
        let plan = build_plan(options(), context()).unwrap();
        let arguments = args(&plan);
        assert!(position(&arguments, "/home/user") < position(&arguments, "/ws"));
    }

    #[test]
    fn network_namespace_is_not_created() {
        let plan = build_plan(options(), context()).unwrap();
        let arguments = args(&plan);
        assert!(!arguments.contains(&"--unshare-net".to_owned()));
    }

    #[test]
    fn runner_binary_is_bound_and_launched() {
        let plan = build_plan(options(), context()).unwrap();
        let arguments = args(&plan);
        let runner = position(&arguments, "/platform/runner");
        assert_eq!(arguments[runner - 1], "--ro-bind");
        assert_eq!(arguments[runner + 1], RUNNER_PATH);
        let separator = position(&arguments, "--");
        assert_eq!(arguments[separator + 1], RUNNER_PATH);
        assert_eq!(arguments[separator + 2], "--stdio");
    }

    #[test]
    fn raw_arguments_precede_the_runner_command() {
        let mut options = options();
        options.bwrap_args = vec![OsString::from("--foo"), OsString::from("bar")];
        let plan = build_plan(options, context()).unwrap();
        let arguments = args(&plan);
        let separator = position(&arguments, "--");
        assert_eq!(arguments[separator - 2], "--foo");
        assert_eq!(arguments[separator - 1], "bar");
    }

    #[test]
    fn explicit_mounts_follow_preset_mounts() {
        let temporary = tempfile::tempdir().unwrap();
        let read_only = temporary.path().join("ro");
        let read_write = temporary.path().join("rw");
        fs::create_dir(&read_only).unwrap();
        fs::create_dir(&read_write).unwrap();
        let mut options = options();
        options.mount_ro = vec![read_only.clone()];
        options.mount_rw = vec![read_write.clone()];
        let plan = build_plan(options, context()).unwrap();
        let arguments = args(&plan);
        let read_only = fs::canonicalize(&read_only).unwrap();
        let read_write = fs::canonicalize(&read_write).unwrap();
        let read_only = position(&arguments, &read_only.to_string_lossy());
        let read_write = position(&arguments, &read_write.to_string_lossy());
        assert!(read_only < read_write);
        assert_eq!(arguments[read_only - 1], "--ro-bind");
        assert_eq!(arguments[read_write - 1], "--bind");
    }

    #[test]
    fn validate_executable_rejects_non_executable_files() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("not-executable");
        fs::write(&file, b"content").unwrap();
        assert!(validate_executable(&file).is_err());
    }

    #[test]
    fn validate_executable_accepts_executable_files() {
        let temporary = tempfile::tempdir().unwrap();
        let file = temporary.path().join("executable");
        fs::write(&file, b"content").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(
            validate_executable(&file).unwrap(),
            fs::canonicalize(&file).unwrap()
        );
    }
}
