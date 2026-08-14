use std::{path::PathBuf, time::Duration};

use atra_protocol::{
    Command, CommandResult, ControllerChange, ControllerLifecycle, SubscriptionTerminal,
    ThreadChange, ThreadEventData, ThreadId,
};
use tempfile::TempDir;
use tokio::{io::AsyncWriteExt, net::UnixStream, task::JoinHandle};

#[tokio::test]
async fn subscriptions_receive_snapshot_then_shared_operations() {
    let controller = TestController::start().await;
    let client = atra_client::Client::new(&controller.endpoint);
    let mut controller_subscription = client.subscribe_controller().await.unwrap();
    assert_eq!(
        controller_subscription.state().lifecycle(),
        ControllerLifecycle::Running
    );
    assert!(controller_subscription.state().threads().is_empty());

    let thread_id = match client
        .command(Command::ThreadCreate {
            display_name: Some("Initial".to_owned()),
        })
        .await
        .unwrap()
    {
        CommandResult::ThreadCreated { thread_id } => thread_id,
        result => panic!("unexpected command result: {result:?}"),
    };
    assert_eq!(
        controller_subscription.receive().await.unwrap(),
        ControllerChange::Thread(thread_id)
    );

    let mut thread_subscription = client.subscribe_thread(thread_id).await.unwrap();
    client
        .command(Command::ThreadRename {
            thread_id,
            display_name: "Renamed".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(
        controller_subscription.receive().await.unwrap(),
        ControllerChange::Thread(thread_id)
    );
    assert_eq!(
        thread_subscription.receive().await.unwrap(),
        ThreadChange::Metadata
    );
    assert_eq!(
        thread_subscription
            .state()
            .metadata()
            .display_name
            .as_deref(),
        Some("Renamed")
    );
}

#[tokio::test]
async fn accepted_turn_progresses_through_thread_operations() {
    let controller = TestController::start().await;
    let client = atra_client::Client::new(&controller.endpoint);
    let thread_id = match client
        .command(Command::ThreadCreate { display_name: None })
        .await
        .unwrap()
    {
        CommandResult::ThreadCreated { thread_id } => thread_id,
        result => panic!("unexpected command result: {result:?}"),
    };
    let mut subscription = client.subscribe_thread(thread_id).await.unwrap();
    assert_eq!(
        client
            .command(Command::ThreadSend {
                thread_id,
                message: "State-synchronized prompt".to_owned(),
                allow_questions: false,
            })
            .await
            .unwrap(),
        CommandResult::Accepted
    );

    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if subscription.receive().await.unwrap() == ThreadChange::TurnFinished {
                break;
            }
        }
    })
    .await
    .expect("turn state did not reach an outcome");
    assert!(subscription.state().active_turn().is_none());
    assert!(subscription.state().last_outcome().is_some());
    assert!(subscription.state().events().iter().any(|event| {
        matches!(
            &event.data,
            ThreadEventData::UserMessage(message)
                if message.content == "State-synchronized prompt"
        )
    }));
}

#[tokio::test]
async fn unsupported_and_unknown_requests_are_rejected_without_stopping_controller() {
    let controller = TestController::start().await;
    for request in [
        br#"{"kind":"unary","request":{"method":"status"}}\n"#.as_slice(),
        br#"{"kind":"subscribe","request":{"resource":"controller","extra":true}}\n"#.as_slice(),
    ] {
        let mut stream = UnixStream::connect(&controller.endpoint).await.unwrap();
        stream.write_all(request).await.unwrap();
        drop(stream);
    }

    let subscription = atra_client::Client::new(&controller.endpoint)
        .subscribe_controller()
        .await
        .unwrap();
    assert_eq!(
        subscription.state().lifecycle(),
        ControllerLifecycle::Running
    );
}

#[tokio::test]
async fn deleting_a_thread_terminates_its_subscription() {
    let controller = TestController::start().await;
    let client = atra_client::Client::new(&controller.endpoint);
    let thread_id = match client
        .command(Command::ThreadCreate { display_name: None })
        .await
        .unwrap()
    {
        CommandResult::ThreadCreated { thread_id } => thread_id,
        result => panic!("unexpected command result: {result:?}"),
    };
    let mut subscription = client.subscribe_thread(thread_id).await.unwrap();

    client
        .command(Command::ThreadDelete { thread_id })
        .await
        .unwrap();
    let error = subscription.receive().await.unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<atra_client::SubscriptionError>()
            .unwrap()
            .terminal(),
        &SubscriptionTerminal::Deleted
    );
}

#[tokio::test]
async fn shutdown_sends_stopping_operation_before_terminal() {
    let controller = TestController::start().await;
    let client = atra_client::Client::new(&controller.endpoint);
    let mut subscription = client.subscribe_controller().await.unwrap();

    assert_eq!(
        client.command(Command::Shutdown).await.unwrap(),
        CommandResult::Accepted
    );
    assert_eq!(
        subscription.receive().await.unwrap(),
        ControllerChange::Lifecycle
    );
    assert_eq!(
        subscription.state().lifecycle(),
        ControllerLifecycle::Stopping
    );
    let error = subscription.receive().await.unwrap_err();
    assert_eq!(
        error
            .downcast_ref::<atra_client::SubscriptionError>()
            .unwrap()
            .terminal(),
        &SubscriptionTerminal::ControllerShutdown
    );
}

#[tokio::test]
async fn missing_subscription_resource_returns_an_error_terminal() {
    let controller = TestController::start().await;
    let error = atra_client::Client::new(&controller.endpoint)
        .subscribe_thread(ThreadId(999))
        .await
        .err()
        .expect("missing thread subscription unexpectedly succeeded");
    assert!(matches!(
        error
            .downcast_ref::<atra_client::SubscriptionError>()
            .unwrap()
            .terminal(),
        SubscriptionTerminal::Error { .. }
    ));
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
        let data_home = directory.path().join("data");
        let server_endpoint = endpoint.clone();
        let task = tokio::spawn(async move {
            atra_controller::run(&server_endpoint, &database, &auth_home, &data_home, None)
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
