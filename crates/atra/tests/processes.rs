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
        format!("oneone-err{}\n", system.workspace.path().display())
    );
    assert!(one.stderr.is_empty());

    assert!(!two.status.success(), "{two:?}");
    assert_eq!(two.stdout, b"twotwo-err");
    let stderr = String::from_utf8(two.stderr).unwrap();
    assert!(
        stderr.contains("command exited with status 7"),
        "{stderr:?}"
    );

    let interactive = system
        .background(
            "one",
            "IFS= read -r line; printf 'received:%s\\n' \"$line\"; sleep 30",
        )
        .await;
    system.write("one", interactive, "hello\n").await;
    let waited = system.wait("one", interactive, 1_000).await;
    assert!(waited.status.success(), "{waited:?}");
    assert_eq!(waited.stdout, b"received:hello\n");
    assert!(
        String::from_utf8(waited.stderr)
            .unwrap()
            .contains("is still running")
    );
    system.stop_process("one", interactive).await;

    let sleeper = system.background("one", "sleep 30").await;
    let mut wait = system
        .atra()
        .args([
            "runner",
            "wait",
            "--name",
            "one",
            "--process-handle",
            &sleeper.to_string(),
            "--timeout-ms",
            "300",
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    sleep(Duration::from_millis(20)).await;
    let concurrent = timeout(
        Duration::from_millis(150),
        system.exec("one", "printf concurrent"),
    )
    .await
    .expect("a wait request blocked another request to the same runner")
    .unwrap();
    assert!(concurrent.status.success(), "{concurrent:?}");
    assert_eq!(concurrent.stdout, b"concurrent");
    assert!(wait.wait().await.unwrap().success());
    system.stop_process("one", sleeper).await;

    let returned = system
        .atra()
        .args([
            "runner",
            "exec",
            "--name",
            "one",
            "--timeout-ms",
            "200",
            "--on-timeout",
            "return-running",
            "--command",
            "printf partial; sleep 30",
        ])
        .output()
        .await
        .unwrap();
    assert!(returned.status.success(), "{returned:?}");
    assert_eq!(returned.stdout, b"partial");
    let returned_handle = String::from_utf8(returned.stderr)
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    system.stop_process("one", returned_handle).await;

    let terminated = system
        .atra()
        .args([
            "runner",
            "exec",
            "--name",
            "one",
            "--timeout-ms",
            "200",
            "--on-timeout",
            "terminate",
            "--command",
            "sleep 30 & printf '%s\\n' \"$!\"; wait",
        ])
        .output()
        .await
        .unwrap();
    assert!(!terminated.status.success(), "{terminated:?}");
    assert!(
        String::from_utf8_lossy(&terminated.stderr).contains("command timed out"),
        "{terminated:?}"
    );
    let descendant = Pid::from_raw(
        String::from_utf8(terminated.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();
    assert!(
        test_kill_process(descendant).is_err(),
        "timeout left a descendant process running"
    );

    let abandoned = system
        .background("two", "sleep 30 & printf '%s\\n' \"$!\"; wait")
        .await;
    let abandoned_output = system.wait("two", abandoned, 1_000).await;
    assert!(abandoned_output.status.success(), "{abandoned_output:?}");
    let abandoned_pid = Pid::from_raw(
        String::from_utf8(abandoned_output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap(),
    )
    .unwrap();

    let process_pids = [
        system.runner_pid("one").await,
        system.runner_pid("two").await,
        abandoned_pid,
    ];
    system.stop().await;

    timeout(Duration::from_secs(1), async {
        loop {
            if process_pids
                .iter()
                .all(|pid| test_kill_process(*pid).is_err())
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runner or managed process survived controller exit");
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

    async fn background(&self, runner: &str, command: &str) -> u64 {
        let output = self
            .atra()
            .args([
                "runner",
                "exec",
                "--name",
                runner,
                "--background",
                "--command",
                command,
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    }

    async fn wait(
        &self,
        runner: &str,
        process_handle: u64,
        timeout_ms: u64,
    ) -> std::process::Output {
        self.atra()
            .args([
                "runner",
                "wait",
                "--name",
                runner,
                "--process-handle",
                &process_handle.to_string(),
                "--timeout-ms",
                &timeout_ms.to_string(),
            ])
            .output()
            .await
            .unwrap()
    }

    async fn write(&self, runner: &str, process_handle: u64, text: &str) {
        let output = self
            .atra()
            .args([
                "runner",
                "write",
                "--name",
                runner,
                "--process-handle",
                &process_handle.to_string(),
                "--text",
                text,
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
    }

    async fn stop_process(&self, runner: &str, process_handle: u64) {
        let output = self
            .atra()
            .args([
                "runner",
                "stop",
                "--name",
                runner,
                "--process-handle",
                &process_handle.to_string(),
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
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
