use std::{process::Stdio, time::Duration};

use rustix::process::{Pid, test_kill_process};
use tempfile::TempDir;
use tokio::{
    process::{Child, Command},
    time::{sleep, timeout},
};

const ATRA: &str = env!("CARGO_BIN_EXE_atra");
const RUNNER: &str = env!("CARGO_BIN_EXE_atra-runner");

#[tokio::test]
async fn two_real_runners_execute_commands_and_exit_with_the_controller() {
    let mut system = TestSystem::start().await;
    system.launch("one").await;
    system.launch("two").await;

    let one = system.exec("one", "printf one; printf one-err >&2; pwd");
    let two = system.exec("two", "printf two; printf two-err >&2; exit 7");
    let (one, two) = tokio::join!(one, two);
    let one = one.unwrap();
    let two = two.unwrap();

    assert!(one.status.success(), "{one:?}");
    assert_eq!(
        String::from_utf8(one.stdout).unwrap(),
        format!("one{}\n", system.workspace.path().display())
    );
    assert_eq!(one.stderr, b"one-err");

    assert!(!two.status.success(), "{two:?}");
    assert_eq!(two.stdout, b"two");
    let stderr = String::from_utf8(two.stderr).unwrap();
    assert!(stderr.starts_with("two-err"), "{stderr:?}");
    assert!(
        stderr.contains("command exited with status 7"),
        "{stderr:?}"
    );

    let runner_pids = [
        system.runner_pid("one").await,
        system.runner_pid("two").await,
    ];
    system.stop().await;

    timeout(Duration::from_secs(1), async {
        loop {
            if runner_pids
                .iter()
                .all(|pid| test_kill_process(*pid).is_err())
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runners survived controller exit");
}

struct TestSystem {
    workspace: TempDir,
    endpoint: std::path::PathBuf,
    controller: Option<Child>,
}

impl TestSystem {
    async fn start() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let endpoint = workspace.path().join("controller.sock");
        let database = workspace.path().join("controller.sqlite3");
        let controller = Command::new(ATRA)
            .args(["controller", "run"])
            .env("ATRA_CONTROLLER_ENDPOINT", &endpoint)
            .env("ATRA_CONTROLLER_STATE", database)
            .current_dir(workspace.path())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        timeout(Duration::from_secs(1), async {
            loop {
                if endpoint.exists() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("controller socket was not created");

        Self {
            workspace,
            endpoint,
            controller: Some(controller),
        }
    }

    async fn launch(&self, name: &str) {
        let output = self
            .atra()
            .args([
                "runner",
                "launch",
                "--name",
                name,
                "--approval",
                "allow",
                "--",
                RUNNER,
                "--stdio",
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"launched\n");
    }

    async fn exec(&self, runner: &str, command: &str) -> std::io::Result<std::process::Output> {
        self.atra()
            .args(["runner", "exec", "--name", runner, "--command", command])
            .output()
            .await
    }

    async fn runner_pid(&self, runner: &str) -> Pid {
        let output = self.exec(runner, "ps -o ppid= -p $$").await.unwrap();
        assert!(output.status.success(), "{output:?}");
        let pid = String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        Pid::from_raw(pid).unwrap()
    }

    async fn stop(&mut self) {
        let mut controller = self.controller.take().unwrap();
        controller.kill().await.unwrap();
        controller.wait().await.unwrap();
    }

    fn atra(&self) -> Command {
        let mut command = Command::new(ATRA);
        command
            .env("ATRA_CONTROLLER_ENDPOINT", &self.endpoint)
            .current_dir(self.workspace.path());
        command
    }
}

impl Drop for TestSystem {
    fn drop(&mut self) {
        if let Some(controller) = &mut self.controller {
            let _ = controller.start_kill();
        }
    }
}
