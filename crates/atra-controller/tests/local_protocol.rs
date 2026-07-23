use std::{path::PathBuf, time::Duration};

use atra_protocol::{ApprovalPolicy, ControllerRequest, ControllerResponse};
use tempfile::TempDir;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    task::JoinHandle,
};

#[tokio::test]
async fn status_reports_running() {
    let controller = TestController::start().await;

    assert_eq!(
        status(&controller.endpoint).await,
        ControllerResponse::Running
    );
}

#[tokio::test]
async fn launching_a_live_runner_is_idempotent() {
    let controller = TestController::start().await;
    let launch = || {
        ControllerRequest::RunnerLaunch {
        name: "test".to_owned(),
        approval: ApprovalPolicy::Ask,
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "IFS= read -r request; printf '%s\\n' '{\"request_id\":0,\"status\":\"ready\"}'; cat >/dev/null"
                .to_owned(),
        ],
    }
    };

    assert_eq!(
        request(&controller.endpoint, launch()).await,
        ControllerResponse::Launched
    );
    assert_eq!(
        request(&controller.endpoint, launch()).await,
        ControllerResponse::AlreadyRunning
    );
}

#[tokio::test]
async fn executes_a_foreground_command_through_a_runner() {
    let controller = TestController::start().await;
    assert_eq!(
        request(
            &controller.endpoint,
            ControllerRequest::RunnerLaunch {
                name: "test".to_owned(),
                approval: ApprovalPolicy::Allow,
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    concat!(
                        "IFS= read -r initialize; ",
                        "printf '%s\\n' '{\"request_id\":0,\"status\":\"ready\"}'; ",
                        "IFS= read -r command; ",
                        "printf '%s\\n' ",
                        "'{\"request_id\":1,\"status\":\"process_finished\",",
                        "\"output\":\"outerr\",\"exit_code\":7}'"
                    )
                    .to_owned(),
                ],
            },
        )
        .await,
        ControllerResponse::Launched
    );

    assert_eq!(
        request(
            &controller.endpoint,
            ControllerRequest::ExecCommand {
                runner: "test".to_owned(),
                command: "printf out; printf err >&2; exit 7".to_owned(),
                cwd: None,
                background: false,
                timeout_ms: None,
                timeout_action: atra_protocol::TimeoutAction::ReturnRunning,
            },
        )
        .await,
        ControllerResponse::ProcessFinished {
            output: "outerr".to_owned(),
            exit_code: Some(7),
        }
    );
}

#[tokio::test]
async fn stalled_client_does_not_block_another_client() {
    let controller = TestController::start().await;
    let _stalled_client = UnixStream::connect(&controller.endpoint).await.unwrap();

    assert_eq!(
        status(&controller.endpoint).await,
        ControllerResponse::Running
    );
}

#[tokio::test]
async fn invalid_message_does_not_stop_controller() {
    let controller = TestController::start().await;
    let mut client = UnixStream::connect(&controller.endpoint).await.unwrap();
    client
        .write_all(b"{\"method\":\"unknown\"}\n")
        .await
        .unwrap();
    drop(client);

    assert_eq!(
        status(&controller.endpoint).await,
        ControllerResponse::Running
    );
}

async fn status(endpoint: &PathBuf) -> ControllerResponse {
    request(endpoint, ControllerRequest::Status).await
}

async fn request(endpoint: &PathBuf, request: ControllerRequest) -> ControllerResponse {
    let mut stream = UnixStream::connect(endpoint).await.unwrap();
    let mut request = serde_json::to_vec(&request).unwrap();
    request.push(b'\n');
    stream.write_all(&request).await.unwrap();

    let mut response = String::new();
    BufReader::new(stream)
        .read_line(&mut response)
        .await
        .unwrap();
    serde_json::from_str(&response).unwrap()
}

struct TestController {
    endpoint: PathBuf,
    task: JoinHandle<()>,
    _directory: TempDir,
}

impl TestController {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let endpoint = directory.path().join("controller.sock");
        let database = directory.path().join("controller.sqlite3");
        let server_endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            atra_controller::run(&server_endpoint, &database)
                .await
                .unwrap();
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if endpoint.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("controller socket was not created");

        Self {
            endpoint,
            task,
            _directory: directory,
        }
    }
}

impl Drop for TestController {
    fn drop(&mut self) {
        self.task.abort();
    }
}
