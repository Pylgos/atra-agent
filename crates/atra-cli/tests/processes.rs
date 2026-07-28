use std::{
    fs,
    process::{ExitStatus, Stdio},
    time::Duration,
};

use atra_protocol::{ThreadEvent, ThreadEventData, ToolResultEvent};
use rustix::process::{Pid, test_kill_process};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, BufReader},
    net::UnixStream,
    process::{Child, ChildStdout, Command},
    time::{sleep, timeout},
};

const ATRA: &str = env!("CARGO_BIN_EXE_atra");

fn tool_result(event: &ThreadEvent) -> serde_json::Value {
    match &event.data {
        ThreadEventData::ToolResult(ToolResultEvent::Custom { result, .. })
        | ThreadEventData::ToolResult(ToolResultEvent::Function { result, .. }) => result.clone(),
        _ => panic!("expected tool result"),
    }
}

#[tokio::test]
async fn two_real_runners_execute_commands_and_exit_with_the_controller() {
    let mut system = TestSystem::start().await;
    system.launch("one", "allow").await;
    system.launch("two", "ask").await;
    let relaunched = system
        .atra()
        .args([
            "runner",
            "launch",
            "--name",
            "one",
            "--description",
            "updated integration test runner",
            "--approval",
            "allow",
        ])
        .output()
        .await
        .unwrap();
    assert!(relaunched.status.success(), "{relaunched:?}");
    assert_eq!(relaunched.stdout, b"already running\n");

    let thread = system.create_thread().await;
    let turn = system
        .send_message(thread, "run the scripted command")
        .await;
    assert!(turn.status.success(), "{turn:?}");
    assert_eq!(turn.stdout, b"observed model-output\n");

    let denied_thread = system.create_thread().await;
    let mut pending = system.start_message(denied_thread, "request a denied command");
    let pending_stdout = pending.approval().await;
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
    let approval = system
        .deny(denied_approval_id, "not in this environment")
        .await;
    assert!(approval.status.success(), "{approval:?}");
    let denied = pending.finish().await;
    assert!(denied.status.success(), "{denied:?}");
    assert_eq!(
        denied.stdout,
        b"denied user denied the tool call: not in this environment\n"
    );
    assert!(!system.workspace.path().join("denied-marker").exists());

    let allowed_thread = system.create_thread().await;
    let mut pending = system.start_message(allowed_thread, "request an approved command");
    let allowed_approval_id = pending
        .approval()
        .await
        .lines()
        .next()
        .unwrap()
        .parse()
        .unwrap();
    let approval = system.allow(allowed_approval_id).await;
    assert!(approval.status.success(), "{approval:?}");
    let allowed = pending.finish().await;
    assert!(allowed.status.success(), "{allowed:?}");
    assert_eq!(allowed.stdout, b"approved approved-output\n");

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
            "if IFS= read -r line; then printf 'received:%s\\n' \"$line\"; else printf 'stdin-eof\\n'; fi; sleep 30",
        )
        .await;
    let waited = system.wait("one", &interactive, 1_000).await;
    assert!(waited.status.success(), "{waited:?}");
    assert_eq!(waited.stdout, b"stdin-eof\n");
    assert!(
        String::from_utf8(waited.stderr)
            .unwrap()
            .contains("is still running")
    );
    system.stop_process("one", &interactive).await;

    let sleeper = system.background("one", "sleep 30").await;
    let mut wait = system
        .atra()
        .args([
            "runner",
            "wait",
            "--thread",
            "1",
            "--name",
            "one",
            "--process-id",
            &sleeper,
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
    system.stop_process("one", &sleeper).await;

    let returned = system
        .atra()
        .args([
            "runner",
            "exec",
            "--thread",
            "1",
            "--name",
            "one",
            "--timeout-ms",
            "200",
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
        .strip_prefix("process \"")
        .and_then(|message| message.strip_suffix("\" is still running\n"))
        .unwrap()
        .to_owned();
    system.stop_process("one", &returned_handle).await;

    let abandoned = system
        .background("two", "sleep 30 & printf '%s\\n' \"$!\"; wait")
        .await;
    let abandoned_output = system.wait("two", &abandoned, 1_000).await;
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
            .map(|event| event.data.kind())
            .collect::<Vec<_>>(),
        [
            "skills",
            "runners",
            "user_message",
            "model_request",
            "model_output",
            "tool_call",
            "tool_result",
            "model_request",
            "model_output",
            "assistant_message"
        ]
    );
    assert_eq!(tool_result(&events[6]), serde_json::json!("model-output"));
    assert_eq!(
        match &events[9].data {
            ThreadEventData::AssistantMessage(message) => message.content.as_str(),
            _ => panic!("expected assistant message"),
        },
        "observed model-output"
    );
    let denied_events = system.events(denied_thread).await;
    assert_eq!(
        denied_events
            .iter()
            .map(|event| event.data.kind())
            .collect::<Vec<_>>(),
        [
            "skills",
            "runners",
            "user_message",
            "model_request",
            "model_output",
            "tool_call",
            "tool_result",
            "model_request",
            "model_output",
            "assistant_message",
        ]
    );
    assert_eq!(
        tool_result(&denied_events[6]),
        serde_json::json!("user denied the tool call: not in this environment")
    );
    let allowed_events = system.events(allowed_thread).await;
    assert_eq!(
        allowed_events
            .iter()
            .map(|event| event.data.kind())
            .collect::<Vec<_>>(),
        [
            "skills",
            "runners",
            "user_message",
            "model_request",
            "model_output",
            "tool_call",
            "tool_result",
            "model_request",
            "model_output",
            "assistant_message",
        ]
    );
    assert_eq!(
        tool_result(&allowed_events[6]),
        serde_json::json!("approved-output")
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

#[derive(Debug)]
struct CompletedTurn {
    status: ExitStatus,
    stdout: Vec<u8>,
}

struct PendingTurn {
    child: Child,
    stdout: BufReader<ChildStdout>,
}

impl PendingTurn {
    async fn approval(&mut self) -> String {
        let mut output = String::new();
        for _ in 0..3 {
            self.stdout.read_line(&mut output).await.unwrap();
        }
        output
    }

    async fn finish(mut self) -> CompletedTurn {
        let mut stdout = Vec::new();
        self.stdout.read_to_end(&mut stdout).await.unwrap();
        let status = self.child.wait().await.unwrap();
        let mut stderr = Vec::new();
        self.child
            .stderr
            .take()
            .unwrap()
            .read_to_end(&mut stderr)
            .await
            .unwrap();
        CompletedTurn { status, stdout }
    }
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
                            "command": "printf model-output",
                            "mode": "foreground",
                            "timeout_ms": 10000
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
                            "command": "printf should-not-run > denied-marker",
                            "mode": "foreground",
                            "timeout_ms": 10000
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
                            "command": "printf approved-output",
                            "mode": "foreground",
                            "timeout_ms": 10000
                        }
                    }
                },
                {
                    "assistant_message": {
                        "content": "approved {{tool_output}}"
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

    fn start_message(&self, thread: i64, message: &str) -> PendingTurn {
        let mut child = self
            .atra()
            .args([
                "thread",
                "send",
                "--thread",
                &thread.to_string(),
                "--message",
                message,
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        PendingTurn {
            stdout: BufReader::new(child.stdout.take().unwrap()),
            child,
        }
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
            .args([
                "runner",
                "exec",
                "--thread",
                "1",
                "--name",
                runner,
                "--command",
                command,
            ])
            .output()
            .await
    }

    async fn background(&self, runner: &str, command: &str) -> String {
        let output = self
            .atra()
            .args([
                "runner",
                "exec",
                "--thread",
                "1",
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
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    async fn wait(
        &self,
        runner: &str,
        process_handle: &str,
        timeout_ms: u64,
    ) -> std::process::Output {
        self.atra()
            .args([
                "runner",
                "wait",
                "--thread",
                "1",
                "--name",
                runner,
                "--process-id",
                process_handle,
                "--timeout-ms",
                &timeout_ms.to_string(),
            ])
            .output()
            .await
            .unwrap()
    }

    async fn stop_process(&self, runner: &str, process_handle: &str) {
        let output = self
            .atra()
            .args([
                "runner",
                "stop",
                "--thread",
                "1",
                "--name",
                runner,
                "--process-id",
                process_handle,
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
