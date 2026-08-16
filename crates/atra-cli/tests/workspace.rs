use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Stdio,
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::{
    net::UnixStream,
    process::{Child, Command},
    time::{sleep, timeout},
};

const ATRA: &str = env!("CARGO_BIN_EXE_atra");

fn workspace_id(workspace: &Path) -> String {
    format!(
        "{:x}",
        Sha256::digest(workspace.as_os_str().as_encoded_bytes())
    )[..16]
        .to_owned()
}

async fn wait_for_controller(endpoint: &Path) {
    timeout(Duration::from_secs(5), async {
        loop {
            if UnixStream::connect(endpoint).await.is_ok() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("controller socket was not created");
}

async fn start_controller(workspace: &Path, endpoint: &Path, database: &Path) -> Child {
    let log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace.join("controller.log"))
        .unwrap();
    let child = Command::new(ATRA)
        .args(["controller", "run"])
        .current_dir(workspace)
        .env("ATRA_CONTROLLER_ENDPOINT", endpoint)
        .env("ATRA_CONTROLLER_STATE", database)
        .env("XDG_DATA_HOME", workspace.join("data"))
        .stderr(Stdio::from(log))
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    wait_for_controller(endpoint).await;
    child
}

#[tokio::test]
async fn workspace_start_launches_setup_runners() {
    let workspace = tempfile::tempdir().unwrap();
    let endpoint = workspace.path().join("controller.sock");
    let database = workspace.path().join("controller.sqlite3");
    fs::create_dir_all(workspace.path().join(".config")).unwrap();
    fs::write(
        workspace.path().join(".config/atra.toml"),
        "builtin_runners = false\nsetup = \"bash .config/atra-setup.bash\"\n",
    )
    .unwrap();
    fs::write(
        workspace.path().join(".config/atra-setup.bash"),
        "\"${ATRA_BINARY}\" runner launch --name test --description \"test runner\" --approval allow\n",
    )
    .unwrap();

    let output = Command::new(ATRA)
        .args(["workspace", "start"])
        .current_dir(workspace.path())
        .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
        .env("ATRA_CONTROLLER_STATE", &database)
        .env("XDG_DATA_HOME", workspace.path().join("data"))
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let list = Command::new(ATRA)
        .args(["runner", "list"])
        .current_dir(workspace.path())
        .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
        .output()
        .await
        .unwrap();
    assert!(list.status.success(), "{list:?}");
    let stdout = String::from_utf8(list.stdout).unwrap();
    assert!(stdout.contains("test"), "{stdout:?}");

    let stop = Command::new(ATRA)
        .args(["controller", "stop"])
        .current_dir(workspace.path())
        .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
        .output()
        .await
        .unwrap();
    assert!(stop.status.success(), "{stop:?}");
}

#[tokio::test]
async fn workspace_clean_removes_only_the_sandbox_home() {
    let workspace = tempfile::tempdir().unwrap();
    let xdg_state = tempfile::tempdir().unwrap();
    let endpoint = workspace.path().join("controller.sock");
    let id = workspace_id(workspace.path());
    let sandbox_dir = xdg_state
        .path()
        .join("atra/workspaces")
        .join(&id)
        .join("sandbox");
    let sandbox_home = sandbox_dir.join("home");
    create_private_directories(xdg_state.path(), &sandbox_home);
    fs::write(sandbox_home.join("marker"), b"content").unwrap();

    let output = Command::new(ATRA)
        .args(["workspace", "clean", "--force"])
        .current_dir(workspace.path())
        .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
        .env("XDG_STATE_HOME", xdg_state.path())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(!sandbox_home.exists());
    assert!(sandbox_dir.exists());
}

#[tokio::test]
async fn workspace_clean_rejects_a_symlinked_workspace_state_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let xdg_state = tempfile::tempdir().unwrap();
    let target = tempfile::tempdir().unwrap();
    let endpoint = workspace.path().join("controller.sock");
    let id = workspace_id(workspace.path());
    let workspaces = xdg_state.path().join("atra/workspaces");
    fs::create_dir_all(&workspaces).unwrap();
    set_private(&xdg_state.path().join("atra"));
    set_private(&workspaces);

    let target_home = target.path().join("sandbox/home");
    create_private_directories(target.path(), &target_home);
    fs::write(target_home.join("marker"), b"content").unwrap();
    symlink(target.path(), workspaces.join(id)).unwrap();

    let output = Command::new(ATRA)
        .args(["workspace", "clean", "--force"])
        .current_dir(workspace.path())
        .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
        .env("XDG_STATE_HOME", xdg_state.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success(), "{output:?}");
    assert!(target_home.join("marker").exists());
}

#[tokio::test]
async fn workspace_clean_rejects_a_running_controller() {
    let workspace = tempfile::tempdir().unwrap();
    let xdg_state = tempfile::tempdir().unwrap();
    let endpoint = workspace.path().join("controller.sock");
    let database = workspace.path().join("controller.sqlite3");
    let mut controller = start_controller(workspace.path(), &endpoint, &database).await;

    let output = Command::new(ATRA)
        .args(["workspace", "clean", "--force"])
        .current_dir(workspace.path())
        .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
        .env("XDG_STATE_HOME", xdg_state.path())
        .output()
        .await
        .unwrap();
    assert!(!output.status.success(), "{output:?}");

    controller.kill().await.unwrap();
    controller.wait().await.unwrap();
}

#[tokio::test]
async fn platform_exec_runs_an_installed_tool() {
    let xdg_data = tempfile::tempdir().unwrap();
    install_synthetic_platform(xdg_data.path());

    let output = Command::new(ATRA)
        .args(["platform", "exec", "hello"])
        .env("XDG_DATA_HOME", xdg_data.path())
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"tool-output\n");
}

fn install_synthetic_platform(xdg_data: &Path) {
    let root = xdg_data.join("atra");
    let store = atra_store::Store::open(root.clone()).unwrap();
    let content = b"#!/bin/sh\necho tool-output\n";
    let digest = atra_store::object_digest(content, true);
    store
        .put_object(&digest, true, std::io::Cursor::new(content))
        .unwrap();
    let manifest = atra_store::TreeManifest {
        entries: vec![atra_store::TreeEntry::File {
            path: "bin/hello".to_owned(),
            object: digest.clone(),
        }],
    };
    let tree_digest = manifest.digest();
    match store.prepare_tree(&manifest).unwrap() {
        atra_store::PreparedTree::Ready { .. } => {}
        atra_store::PreparedTree::MissingObjects(_) => panic!("tree was not ready"),
    }
    let platform = format!("{}-linux-static", std::env::consts::ARCH);
    let platform_directory = root.join("platforms").join(&platform);
    fs::create_dir_all(&platform_directory).unwrap();
    let profile = serde_json::json!({ "runner": digest, "tools": tree_digest });
    fs::write(
        platform_directory.join("default.json"),
        serde_json::to_vec(&profile).unwrap(),
    )
    .unwrap();
}

fn create_private_directories(root: &Path, leaf: &Path) {
    fs::create_dir_all(leaf).unwrap();
    let mut current = leaf;
    while current != root {
        set_private(current);
        current = current.parent().unwrap();
    }
}

fn set_private(path: &Path) {
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
