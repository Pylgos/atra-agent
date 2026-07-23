use std::{path::PathBuf, time::Duration};

use atra_protocol::{ControllerRequest, ControllerResponse};
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
    let mut stream = UnixStream::connect(endpoint).await.unwrap();
    let mut request = serde_json::to_vec(&ControllerRequest::Status).unwrap();
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
