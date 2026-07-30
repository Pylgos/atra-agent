use std::{path::PathBuf, time::Duration};

use atra_protocol::{
    ControllerRequest, ControllerResponse, Thread, ThreadEventData, ThreadId, TurnRequest,
    UnaryRequest,
};
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
async fn lists_threads_newest_first() {
    let controller = TestController::start().await;
    let first = request(
        &controller.endpoint,
        unary(UnaryRequest::ThreadCreate {
            display_name: Some("Named".to_owned()),
        }),
    )
    .await;
    let second = request(
        &controller.endpoint,
        unary(UnaryRequest::ThreadCreate { display_name: None }),
    )
    .await;

    assert_eq!(
        (first, second),
        (
            ControllerResponse::ThreadCreated {
                thread_id: ThreadId(1),
            },
            ControllerResponse::ThreadCreated {
                thread_id: ThreadId(2),
            },
        )
    );
    assert_eq!(
        request(&controller.endpoint, unary(UnaryRequest::ThreadList)).await,
        ControllerResponse::ThreadList {
            threads: vec![
                Thread {
                    id: ThreadId(2),
                    display_name: None,
                    model: "gpt-5.6-sol".to_owned(),
                    reasoning_effort: "medium".to_owned(),
                },
                Thread {
                    id: ThreadId(1),
                    display_name: Some("Named".to_owned()),
                    model: "gpt-5.6-sol".to_owned(),
                    reasoning_effort: "medium".to_owned(),
                }
            ]
        }
    );

    assert_eq!(
        request(
            &controller.endpoint,
            unary(UnaryRequest::ThreadRename {
                thread_id: ThreadId(1),
                display_name: "Renamed".to_owned(),
            }),
        )
        .await,
        ControllerResponse::ThreadRenamed
    );
    let mut stream = UnixStream::connect(&controller.endpoint).await.unwrap();
    let mut encoded_request = serde_json::to_vec(&turn(TurnRequest::ThreadSend {
        thread_id: ThreadId(2),
        message: "First prompt".to_owned(),
    }))
    .unwrap();
    encoded_request.push(b'\n');
    stream.write_all(&encoded_request).await.unwrap();
    let mut responses = BufReader::new(stream).lines();
    assert_eq!(
        serde_json::from_str::<ControllerResponse>(&responses.next_line().await.unwrap().unwrap())
            .unwrap(),
        ControllerResponse::TurnStarted {
            thread_id: ThreadId(2),
        }
    );
    let skills =
        serde_json::from_str::<ControllerResponse>(&responses.next_line().await.unwrap().unwrap())
            .unwrap();
    assert!(matches!(
        skills,
        ControllerResponse::TurnEvent { event } if matches!(event.data, ThreadEventData::Skills(_))
    ));
    let runners =
        serde_json::from_str::<ControllerResponse>(&responses.next_line().await.unwrap().unwrap())
            .unwrap();
    assert!(matches!(
        runners,
        ControllerResponse::TurnEvent { event } if matches!(event.data, ThreadEventData::Runners(_))
    ));
    let event =
        serde_json::from_str::<ControllerResponse>(&responses.next_line().await.unwrap().unwrap())
            .unwrap();
    assert!(matches!(
        event,
        ControllerResponse::TurnEvent { event }
            if matches!(&event.data, ThreadEventData::UserMessage(message) if message.content == "First prompt")
    ));
    let response =
        serde_json::from_str::<ControllerResponse>(&responses.next_line().await.unwrap().unwrap())
            .unwrap();
    assert!(matches!(response, ControllerResponse::Error { .. }));
    assert_eq!(
        request(&controller.endpoint, unary(UnaryRequest::ThreadList)).await,
        ControllerResponse::ThreadList {
            threads: vec![
                Thread {
                    id: ThreadId(2),
                    display_name: Some("First prompt".to_owned()),
                    model: "gpt-5.6-sol".to_owned(),
                    reasoning_effort: "medium".to_owned(),
                },
                Thread {
                    id: ThreadId(1),
                    display_name: Some("Renamed".to_owned()),
                    model: "gpt-5.6-sol".to_owned(),
                    reasoning_effort: "medium".to_owned(),
                }
            ]
        }
    );
}

#[tokio::test]
async fn active_turn_can_be_cancelled_after_starting() {
    let controller = TestController::start().await;
    assert_eq!(
        request(
            &controller.endpoint,
            unary(UnaryRequest::ThreadCreate { display_name: None }),
        )
        .await,
        ControllerResponse::ThreadCreated {
            thread_id: ThreadId(1),
        }
    );
    let mut stream = UnixStream::connect(&controller.endpoint).await.unwrap();
    let mut encoded_request = serde_json::to_vec(&turn(TurnRequest::ThreadSend {
        thread_id: ThreadId(1),
        message: "Cancel this".to_owned(),
    }))
    .unwrap();
    encoded_request.push(b'\n');
    stream.write_all(&encoded_request).await.unwrap();
    let mut responses = BufReader::new(stream).lines();
    assert_eq!(
        serde_json::from_str::<ControllerResponse>(&responses.next_line().await.unwrap().unwrap())
            .unwrap(),
        ControllerResponse::TurnStarted {
            thread_id: ThreadId(1),
        }
    );

    assert_eq!(
        tokio::time::timeout(
            Duration::from_secs(1),
            request(
                &controller.endpoint,
                unary(UnaryRequest::ThreadCancel {
                    thread_id: ThreadId(1),
                }),
            ),
        )
        .await
        .expect("cancellation did not complete"),
        ControllerResponse::ThreadCancelled
    );
    let terminal = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let response = serde_json::from_str::<ControllerResponse>(
                &responses.next_line().await.unwrap().unwrap(),
            )
            .unwrap();
            if matches!(
                response,
                ControllerResponse::ThreadCancelled | ControllerResponse::Error { .. }
            ) {
                break response;
            }
        }
    })
    .await
    .expect("turn stream did not complete");
    assert_eq!(terminal, ControllerResponse::ThreadCancelled);
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
    request(endpoint, unary(UnaryRequest::Status)).await
}

fn unary(request: UnaryRequest) -> ControllerRequest {
    ControllerRequest::Unary(request)
}

fn turn(request: TurnRequest) -> ControllerRequest {
    ControllerRequest::Turn(request)
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
        let auth_home = directory.path().join("auth");
        let server_endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            atra_controller::run(&server_endpoint, &database, &auth_home, None)
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
