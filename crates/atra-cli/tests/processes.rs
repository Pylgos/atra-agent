use std::{fs, process::Stdio, time::Duration};

use atra_protocol::ThreadEvent;
use rustix::process::{Pid, test_kill_process};
use tempfile::TempDir;
use tokio::{
    net::UnixStream,
    process::{Child, Command},
    time::{sleep, timeout},
};

const ATRA: &str = env!("CARGO_BIN_EXE_atra");

#[tokio::test]
async fn two_real_runners_execute_commands_and_exit_with_the_controller() {
    let mut system = TestSystem::start().await;
    system.launch("one", "allow").await;
    system.launch("two", "ask").await;

    let thread = system.create_thread().await;
    let turn = system
        .send_message(thread, "run the scripted command")
        .await;
    assert!(turn.status.success(), "{turn:?}");
    assert_eq!(
        turn.stdout,
        b"observed model-output\natra exec_command: process finished with exit code 0\n"
    );

    let denied_thread = system.create_thread().await;
    let pending = system
        .send_message(denied_thread, "request a denied command")
        .await;
    assert!(pending.status.success(), "{pending:?}");
    let pending_stdout = String::from_utf8(pending.stdout).unwrap();
    let denied_approval_id = pending_stdout.lines().next().unwrap().parse().unwrap();
    assert!(
        pending_stdout.contains("tool: exec_command"),
        "{pending_stdout:?}"
    );
    assert!(
        pending_stdout.contains("\"runner\":\"two\"")
            && pending_stdout.contains("printf should-not-run > denied-marker"),
        "{pending_stdout:?}"
    );
    let denied = system
        .deny(denied_approval_id, "not in this environment")
        .await;
    assert!(denied.status.success(), "{denied:?}");
    assert_eq!(
        denied.stdout,
        b"denied user denied the tool call: not in this environment\n"
    );
    assert!(!system.workspace.path().join("denied-marker").exists());

    let allowed_thread = system.create_thread().await;
    let pending = system
        .send_message(allowed_thread, "request an approved command")
        .await;
    assert!(pending.status.success(), "{pending:?}");
    let allowed_approval_id = String::from_utf8(pending.stdout)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let allowed = system.allow(allowed_approval_id).await;
    assert!(allowed.status.success(), "{allowed:?}");
    assert_eq!(
        allowed.stdout,
        b"approved approved-output\natra exec_command: process finished with exit code 0\n"
    );

    fs::write(
        system.workspace.path().join("patch-target.txt"),
        "alpha\n  old spacing\nmiddle one\nmiddle two\nomega\n",
    )
    .unwrap();
    fs::write(system.workspace.path().join("delete-me.txt"), "obsolete\n").unwrap();
    fs::write(system.workspace.path().join("move-me.txt"), "old tail\n").unwrap();
    let patch = "*** Begin Patch\n\
*** Update File: patch-target.txt\n\
@@\n\
-old spacing\n\
+new spacing\n\
@ start 3\n\
-middle one\n\
@ end 4\n\
-middle two\n\
+middle replacement\n\
*** Add File: added.txt\n\
+added\n\
*** Delete File: delete-me.txt\n\
*** Update File: move-me.txt\n\
*** Move to: moved.txt\n\
@@\n\
-old tail\n\
+new tail\n\
*** End of File\n\
\n\
*** End Patch\n";
    let patched = system.apply_patch("one", patch).await;
    assert!(patched.status.success(), "{patched:?}");
    assert_eq!(
        fs::read_to_string(system.workspace.path().join("patch-target.txt")).unwrap(),
        "alpha\nnew spacing\nmiddle replacement\nomega\n"
    );
    assert_eq!(
        fs::read_to_string(system.workspace.path().join("added.txt")).unwrap(),
        "added\n"
    );
    assert!(!system.workspace.path().join("delete-me.txt").exists());
    assert!(!system.workspace.path().join("move-me.txt").exists());
    assert_eq!(
        fs::read_to_string(system.workspace.path().join("moved.txt")).unwrap(),
        "new tail\n"
    );
    let partial = system
        .apply_patch(
            "one",
            "*** Begin Patch\n\
*** Add File: applied-before-error.txt\n\
+kept\n\
*** Update File: missing.txt\n\
@@\n\
-missing\n\
+changed\n\
*** End Patch\n",
        )
        .await;
    assert!(!partial.status.success(), "{partial:?}");
    assert_eq!(
        fs::read_to_string(system.workspace.path().join("applied-before-error.txt")).unwrap(),
        "kept\n"
    );

    let patch_thread = system.create_thread().await;
    let pending = system
        .send_message(patch_thread, "request an approved patch")
        .await;
    assert!(pending.status.success(), "{pending:?}");
    let pending_stdout = String::from_utf8(pending.stdout).unwrap();
    assert!(
        pending_stdout.contains("tool: apply_patch"),
        "{pending_stdout:?}"
    );
    let patch_approval_id = pending_stdout.lines().next().unwrap().parse().unwrap();
    let approved_patch = system.allow(patch_approval_id).await;
    assert!(approved_patch.status.success(), "{approved_patch:?}");
    assert_eq!(approved_patch.stdout, b"patched patch-ok\n");
    assert_eq!(
        fs::read_to_string(system.workspace.path().join("model-patched.txt")).unwrap(),
        "patch-ok\n"
    );

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

    system.start_controller().await;
    let events = system.events(thread).await;
    assert_eq!(
        events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "user_message",
            "model_request",
            "tool_call",
            "tool_result",
            "model_request",
            "assistant_message"
        ]
    );
    assert_eq!(
        events[3].payload["result"],
        serde_json::json!("model-output\natra exec_command: process finished with exit code 0")
    );
    assert_eq!(
        events[5].payload["content"],
        serde_json::json!(
            "observed model-output\natra exec_command: process finished with exit code 0"
        )
    );
    let denied_events = system.events(denied_thread).await;
    assert_eq!(
        denied_events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "user_message",
            "model_request",
            "tool_call",
            "approval_request",
            "approval_response",
            "tool_result",
            "model_request",
            "assistant_message",
        ]
    );
    assert_eq!(
        denied_events[4].payload,
        serde_json::json!({
            "approval_id": denied_approval_id,
            "decision": "deny",
            "reason": "not in this environment",
        })
    );
    let allowed_events = system.events(allowed_thread).await;
    assert_eq!(
        allowed_events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "user_message",
            "model_request",
            "tool_call",
            "approval_request",
            "approval_response",
            "tool_result",
            "model_request",
            "assistant_message",
        ]
    );
    assert_eq!(
        allowed_events[4].payload,
        serde_json::json!({
            "approval_id": allowed_approval_id,
            "decision": "allow",
            "reason": null,
        })
    );
    assert_eq!(
        allowed_events[5].payload["result"],
        serde_json::json!("approved-output\natra exec_command: process finished with exit code 0")
    );
}

struct TestSystem {
    workspace: TempDir,
    endpoint: std::path::PathBuf,
    database: std::path::PathBuf,
    model_script: std::path::PathBuf,
    controller_log: std::path::PathBuf,
    controller: Option<Child>,
}

impl TestSystem {
    async fn start() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let endpoint = workspace.path().join("controller.sock");
        let database = workspace.path().join("controller.sqlite3");
        let model_script = workspace.path().join("model.json");
        let controller_log = workspace.path().join("controller.log");
        fs::write(
            &model_script,
            r#"[
                {
                    "tool_call": {
                        "name": "exec_command",
                        "arguments": {
                            "runner": "one",
                            "command": "printf model-output"
                        }
                    }
                },
                {
                    "assistant_message": {
                        "content": "observed {{tool_output}}"
                    }
                },
                {
                    "tool_call": {
                        "name": "exec_command",
                        "arguments": {
                            "runner": "two",
                            "command": "printf should-not-run > denied-marker"
                        }
                    }
                },
                {
                    "assistant_message": {
                        "content": "denied {{tool_output}}"
                    }
                },
                {
                    "tool_call": {
                        "name": "exec_command",
                        "arguments": {
                            "runner": "two",
                            "command": "printf approved-output"
                        }
                    }
                },
                {
                    "assistant_message": {
                        "content": "approved {{tool_output}}"
                    }
                },
                {
                    "tool_call": {
                        "name": "apply_patch",
                        "arguments": {
                            "runner": "two",
                            "patch": "*** Begin Patch\n*** Add File: model-patched.txt\n+patch-ok\n*** End Patch"
                        }
                    }
                },
                {
                    "assistant_message": {
                        "content": "patched patch-ok"
                    }
                }
            ]"#,
        )
        .unwrap();
        let mut system = Self {
            workspace,
            endpoint,
            database,
            model_script,
            controller_log,
            controller: None,
        };
        system.start_controller().await;
        system
    }

    async fn start_controller(&mut self) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.controller_log)
            .unwrap();
        let controller = Command::new(ATRA)
            .args(["controller", "run"])
            .env("ATRA_CONTROLLER_ENDPOINT", &self.endpoint)
            .env("ATRA_CONTROLLER_STATE", &self.database)
            .env("ATRA_FAKE_MODEL_SCRIPT", &self.model_script)
            .current_dir(self.workspace.path())
            .stderr(Stdio::from(log))
            .kill_on_drop(true)
            .spawn()
            .unwrap();

        timeout(Duration::from_secs(1), async {
            loop {
                if UnixStream::connect(&self.endpoint).await.is_ok() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("controller socket was not created");
        self.controller = Some(controller);
    }

    async fn launch(&self, name: &str, approval: &str) {
        let output = self
            .atra()
            .args([
                "runner",
                "launch",
                "--name",
                name,
                "--description",
                "integration test runner",
                "--approval",
                approval,
            ])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"launched\n");
    }

    async fn create_thread(&self) -> i64 {
        let output = self
            .atra()
            .args(["thread", "create"])
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

    async fn apply_patch(&self, runner: &str, patch: &str) -> std::process::Output {
        let mut child = self
            .atra()
            .args(["runner", "apply-patch", "--name", runner])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        use tokio::io::AsyncWriteExt;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(patch.as_bytes())
            .await
            .unwrap();
        child.wait_with_output().await.unwrap()
    }

    async fn send_message(&self, thread: i64, message: &str) -> std::process::Output {
        self.atra()
            .args([
                "thread",
                "send",
                "--thread",
                &thread.to_string(),
                "--message",
                message,
            ])
            .output()
            .await
            .unwrap()
    }

    async fn deny(&self, approval: u64, reason: &str) -> std::process::Output {
        self.atra()
            .args([
                "approval",
                "deny",
                "--approval",
                &approval.to_string(),
                "--reason",
                reason,
            ])
            .output()
            .await
            .unwrap()
    }

    async fn allow(&self, approval: u64) -> std::process::Output {
        self.atra()
            .args(["approval", "allow", "--approval", &approval.to_string()])
            .output()
            .await
            .unwrap()
    }

    async fn events(&self, thread: i64) -> Vec<ThreadEvent> {
        let output = self
            .atra()
            .args(["thread", "events", "--thread", &thread.to_string()])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
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
        if std::thread::panicking()
            && let Ok(log) = fs::read_to_string(&self.controller_log)
            && !log.is_empty()
        {
            eprintln!("controller log:\n{log}");
        }
    }
}
