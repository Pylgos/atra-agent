use std::{
    collections::HashMap,
    convert::Infallible,
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use atra_client::Client;
use atra_protocol::{
    AgentStatus, CheckpointId, CheckpointSubscriptionMessage, Command, ControllerOperation,
    ControllerSubscriptionMessage, ProcessId, ProcessLocator, ProcessSubscriptionMessage,
    SubscriptionTerminal, ThreadId, ThreadSubscriptionMessage,
};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{HeaderMap, Request, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post, put},
};
use futures_util::Stream;
use rustix::process::getuid;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;

mod push;
use push::{PushManager, PushPayload, PushSubscription, PushTestRequest};

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

#[derive(Clone)]
struct AppState {
    runtime: PathBuf,
    authority: String,
    pid: u32,
    push: PushManager,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceMetadata {
    workspace_id: String,
    path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct Workspace {
    pub workspace_id: String,
    pub name: String,
    pub path: PathBuf,
    #[serde(skip)]
    endpoint: PathBuf,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    fn unavailable(error: anyhow::Error) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, format!("{error:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({"error": self.message}))).into_response()
    }
}

fn router(runtime: PathBuf, push: PushManager, port: u16) -> Router {
    let state = AppState {
        runtime,
        authority: format!("127.0.0.1:{port}"),
        pid: std::process::id(),
        push,
    };
    Router::new()
        .route("/health", get(health))
        .route("/api/workspaces", get(workspaces))
        .route("/api/workspaces/events", get(workspace_events))
        .route(
            "/api/workspaces/{workspace}/controller/events",
            get(controller_events),
        )
        .route(
            "/api/workspaces/{workspace}/threads/{thread}/events",
            get(thread_events),
        )
        .route(
            "/api/workspaces/{workspace}/threads/{thread}/checkpoints/{checkpoint}/events",
            get(checkpoint_events),
        )
        .route(
            "/api/workspaces/{workspace}/runners/{runner}/processes/{process}/events",
            get(process_events),
        )
        .route("/api/workspaces/{workspace}/commands", post(commands))
        .route("/api/push/key", get(push_key))
        .route(
            "/api/push/subscription",
            put(push_subscribe).delete(push_unsubscribe),
        )
        .route("/api/push/test", post(push_test))
        .fallback(get(asset))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_expected_host,
        ))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({"service": "atra-web", "pid": state.pid}))
}

async fn require_expected_host(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let header_bytes = request
        .headers()
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len())
        .sum::<usize>();
    if request.headers().len() > 64 || header_bytes > 32 * 1024 {
        return ApiError::new(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "request headers are too large",
        )
        .into_response();
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if host != Some(state.authority.as_str()) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "Host does not match the Web daemon authority",
        )
        .into_response();
    }
    next.run(request).await
}

async fn asset(uri: axum::http::Uri) -> Response {
    let requested = uri.path();
    if requested.split('/').any(|part| part == "..") || requested.contains('\\') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let normalized = format!(
        "/{}",
        requested
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
            .collect::<Vec<_>>()
            .join("/")
    );
    let asset_path = if normalized == "/" {
        "/index.html"
    } else {
        normalized.as_str()
    };
    if let Some((bytes, content_type)) = embedded::get(asset_path) {
        let cache = if matches!(asset_path, "/index.html" | "/service-worker.js") {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        };
        return (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, cache),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response();
    }
    if normalized
        .rsplit('/')
        .next()
        .is_some_and(|name| name.contains('.'))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    if let Some((bytes, content_type)) = embedded::get("/index.html") {
        return (
            [
                (header::CONTENT_TYPE, content_type),
                (header::CACHE_CONTROL, "no-cache"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

pub async fn serve(
    listener: TcpListener,
    runtime: PathBuf,
    push_state_path: PathBuf,
) -> Result<()> {
    let port = listener.local_addr()?.port();
    let push = PushManager::open(push_state_path)?;
    let watchers = spawn_push_watchers(runtime.clone(), push.clone());
    let result = axum::serve(listener, router(runtime, push, port))
        .await
        .context("Web daemon failed");
    watchers.abort();
    result
}

async fn push_key(State(state): State<AppState>) -> Result<Json<serde_json::Value>, ApiError> {
    let key = state
        .push
        .public_key()
        .await
        .map_err(ApiError::unavailable)?;
    Ok(Json(json!({"public_key": key})))
}

async fn push_subscribe(
    State(state): State<AppState>,
    Json(subscription): Json<PushSubscription>,
) -> Result<StatusCode, ApiError> {
    state
        .push
        .subscribe(subscription)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_REQUEST, format!("{error:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PushUnsubscribeRequest {
    endpoint: String,
}

async fn push_unsubscribe(
    State(state): State<AppState>,
    Json(request): Json<PushUnsubscribeRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .push
        .unsubscribe(&request.endpoint)
        .await
        .map_err(ApiError::unavailable)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn push_test(
    State(state): State<AppState>,
    Json(request): Json<PushTestRequest>,
) -> Result<StatusCode, ApiError> {
    state
        .push
        .send_test(&request.endpoint)
        .await
        .map_err(|error| ApiError::new(StatusCode::BAD_GATEWAY, format!("{error:#}")))?;
    Ok(StatusCode::NO_CONTENT)
}

fn spawn_push_watchers(runtime: PathBuf, push: PushManager) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut watchers = HashMap::new();
        loop {
            watchers.retain(|_, task: &mut tokio::task::JoinHandle<()>| !task.is_finished());
            for workspace in discover(&runtime).await {
                if watchers.contains_key(&workspace.workspace_id) {
                    continue;
                }
                let id = workspace.workspace_id.clone();
                let manager = push.clone();
                watchers.insert(id, tokio::spawn(watch_workspace(workspace, manager)));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    })
}

async fn watch_workspace(workspace: Workspace, push: PushManager) {
    let client = Client::new(&workspace.endpoint);
    let Ok(mut subscription) = client.subscribe_controller().await else {
        return;
    };
    loop {
        let Ok((operation, _)) = subscription.receive_operation().await else {
            return;
        };
        let ControllerOperation::ThreadStatusUpdated { thread_id, status } = operation else {
            continue;
        };
        let Some((title, status_name)) = notification_status(status) else {
            continue;
        };
        let thread_name = subscription
            .state()
            .threads()
            .iter()
            .find(|thread| thread.id == thread_id)
            .and_then(|thread| thread.display_name.as_deref())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("Thread {thread_id}"));
        push.send_all(&PushPayload {
            title: title.to_owned(),
            body: format!("{} · {thread_name}", workspace.name),
            tag: format!("atra-{}-{thread_id}-{status_name}", workspace.workspace_id),
            url: format!("/w/{}/threads/{thread_id}", workspace.workspace_id),
        })
        .await;
    }
}

fn notification_status(status: AgentStatus) -> Option<(&'static str, &'static str)> {
    match status {
        AgentStatus::AwaitingApproval => Some(("Approval required", "approval")),
        AgentStatus::AwaitingQuestion => Some(("Question waiting", "question")),
        AgentStatus::Completed => Some(("Agent completed", "completed")),
        AgentStatus::Failed => Some(("Agent failed", "failed")),
        _ => None,
    }
}

async fn workspaces(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({"workspaces": discover(&state.runtime).await}))
}

async fn workspace_events(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = async_stream::stream! {
        loop {
            let value = json!({"workspaces": discover(&state.runtime).await});
            yield Ok(Event::default().data(value.to_string()));
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn controller_events(
    State(state): State<AppState>,
    AxumPath(workspace): AxumPath<String>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let client = workspace_client(&state.runtime, &workspace).await?;
    let mut subscription = client
        .subscribe_controller()
        .await
        .map_err(ApiError::unavailable)?;
    let snapshot = ControllerSubscriptionMessage::Snapshot {
        state: subscription.state().clone(),
    };
    let stream = async_stream::stream! {
        yield Ok(Event::default().data(serde_json::to_string(&snapshot).unwrap()));
        loop {
            match subscription.receive_operation().await {
                Ok((operation, _)) => {
                    let message = ControllerSubscriptionMessage::Operation { operation };
                    yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
                }
                Err(error) => {
                    let message = ControllerSubscriptionMessage::Terminal {
                        terminal: SubscriptionTerminal::Error { message: format!("{error:#}") }
                    };
                    yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
                    break;
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn thread_events(
    State(state): State<AppState>,
    AxumPath((workspace, thread)): AxumPath<(String, i64)>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let client = workspace_client(&state.runtime, &workspace).await?;
    let mut subscription = client
        .subscribe_thread(ThreadId(thread))
        .await
        .map_err(ApiError::unavailable)?;
    let snapshot = ThreadSubscriptionMessage::Snapshot {
        state: subscription.state().clone(),
    };
    let stream = async_stream::stream! {
        yield Ok(Event::default().data(serde_json::to_string(&snapshot).unwrap()));
        loop {
            match subscription.receive_operation().await {
                Ok((operation, _)) => {
                    let message = ThreadSubscriptionMessage::Operation { operation };
                    yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
                }
                Err(error) => {
                    let message = ThreadSubscriptionMessage::Terminal {
                        terminal: SubscriptionTerminal::Error { message: format!("{error:#}") }
                    };
                    yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
                    break;
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn checkpoint_events(
    State(state): State<AppState>,
    AxumPath((workspace, _thread, checkpoint)): AxumPath<(String, i64, i64)>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let client = workspace_client(&state.runtime, &workspace).await?;
    let mut subscription = client
        .subscribe_checkpoint(CheckpointId(checkpoint))
        .await
        .map_err(ApiError::unavailable)?;
    if subscription.state().metadata().thread_id != ThreadId(_thread) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "checkpoint does not belong to the requested Thread",
        ));
    }
    let snapshot = CheckpointSubscriptionMessage::Snapshot {
        state: subscription.state().clone(),
    };
    let stream = async_stream::stream! {
        yield Ok(Event::default().data(serde_json::to_string(&snapshot).unwrap()));
        if let Err(error) = subscription.receive_terminal().await {
            let message = CheckpointSubscriptionMessage::Terminal {
                terminal: SubscriptionTerminal::Error { message: format!("{error:#}") }
            };
            yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct ProcessQuery {
    thread_id: i64,
}

async fn process_events(
    State(state): State<AppState>,
    AxumPath((workspace, runner, process)): AxumPath<(String, String, String)>,
    Query(query): Query<ProcessQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let client = workspace_client(&state.runtime, &workspace).await?;
    let locator = ProcessLocator::new(ThreadId(query.thread_id), runner, ProcessId(process));
    let mut subscription = client
        .subscribe_process(locator)
        .await
        .map_err(ApiError::unavailable)?;
    let snapshot = ProcessSubscriptionMessage::Snapshot {
        state: subscription.state().clone(),
    };
    let stream = async_stream::stream! {
        yield Ok(Event::default().data(serde_json::to_string(&snapshot).unwrap()));
        loop {
            match subscription.receive_operation().await {
                Ok((operation, _)) => {
                    let message = ProcessSubscriptionMessage::Operation { operation };
                    yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
                }
                Err(error) => {
                    let message = ProcessSubscriptionMessage::Terminal {
                        terminal: SubscriptionTerminal::Error { message: format!("{error:#}") }
                    };
                    yield Ok(Event::default().data(serde_json::to_string(&message).unwrap()));
                    break;
                }
            }
        }
    };
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn commands(
    State(state): State<AppState>,
    AxumPath(workspace): AxumPath<String>,
    headers: HeaderMap,
    Json(command): Json<Command>,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_command_headers(&state.authority, &headers)?;
    let client = workspace_client(&state.runtime, &workspace).await?;
    let result = client
        .command(command)
        .await
        .map_err(|error| ApiError::new(StatusCode::CONFLICT, format!("{error:#}")))?;
    serde_json::to_value(result)
        .map(Json)
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

fn check_command_headers(authority: &str, headers: &HeaderMap) -> Result<(), ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    if host != Some(authority) {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Host does not match the Web daemon origin",
        ));
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(|value| {
        value.eq_ignore_ascii_case("application/json")
            || value.to_ascii_lowercase().starts_with("application/json;")
    }) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "commands require application/json",
        ));
    }
    Ok(())
}

async fn workspace_client(runtime: &Path, id: &str) -> Result<Client, ApiError> {
    discover(runtime)
        .await
        .into_iter()
        .find(|workspace| workspace.workspace_id == id)
        .map(|workspace| Client::new(workspace.endpoint))
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "running Workspace not found"))
}

async fn discover(runtime: &Path) -> Vec<Workspace> {
    if !private(runtime, 0o700, true) {
        return Vec::new();
    }
    let Ok(entries) = fs::read_dir(runtime) else {
        return Vec::new();
    };
    let mut workspaces = Vec::new();
    for entry in entries.flatten() {
        if let Some(workspace) = validate_workspace(entry.path()).await {
            workspaces.push(workspace);
        }
    }
    workspaces.sort_by(|left, right| left.path.cmp(&right.path));
    workspaces
}

async fn validate_workspace(directory: PathBuf) -> Option<Workspace> {
    if !private(&directory, 0o700, true) {
        return None;
    }
    let metadata_path = directory.join("workspace.json");
    if !private(&metadata_path, 0o600, false) {
        return None;
    }
    let metadata: WorkspaceMetadata =
        serde_json::from_slice(&fs::read(metadata_path).ok()?).ok()?;
    if directory.file_name()?.to_str()? != metadata.workspace_id {
        return None;
    }
    let canonical = fs::canonicalize(&metadata.path).ok()?;
    if canonical != metadata.path {
        return None;
    }
    let endpoint = directory.join("controller.sock");
    if !private_socket(&endpoint) {
        return None;
    }
    Client::new(&endpoint).subscribe_controller().await.ok()?;
    let name = canonical.file_name()?.to_string_lossy().into_owned();
    Some(Workspace {
        workspace_id: metadata.workspace_id,
        name,
        path: canonical,
        endpoint,
    })
}

fn private_socket(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_socket()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == getuid().as_raw()
}

fn private(path: &Path, mode: u32, directory: bool) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    !metadata.file_type().is_symlink()
        && if directory {
            metadata.is_dir()
        } else {
            metadata.is_file()
        }
        && metadata.uid() == getuid().as_raw()
        && metadata.mode() & 0o777 == mode
}

#[cfg(test)]
mod tests {
    use super::*;
    use atra_protocol::CommandResult;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    use futures_util::StreamExt;
    use std::os::unix::fs::PermissionsExt;
    use web_push_native::p256::{
        SecretKey,
        elliptic_curve::{rand_core::OsRng, sec1::ToEncodedPoint},
    };

    #[test]
    fn command_security_requires_exact_host_and_json() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:2872".parse().unwrap());
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        assert!(check_command_headers("127.0.0.1:2872", &headers).is_ok());
        headers.insert(header::HOST, "localhost:2872".parse().unwrap());
        assert!(check_command_headers("127.0.0.1:2872", &headers).is_err());
        headers.insert(header::HOST, "127.0.0.1:2872".parse().unwrap());
        headers.insert(header::CONTENT_TYPE, "text/plain".parse().unwrap());
        assert!(check_command_headers("127.0.0.1:2872", &headers).is_err());
    }

    #[tokio::test]
    async fn command_endpoint_accepts_a_public_origin_from_a_loopback_proxy() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            runtime.path().to_owned(),
            runtime.path().join("push.json"),
        ));
        let response = reqwest::Client::new()
            .post(format!("http://{address}/api/workspaces/missing/commands"))
            .header(header::ORIGIN, "https://atra.example.com")
            .json(&Command::Shutdown)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        task.abort();
    }

    #[tokio::test]
    async fn embedded_application_is_served_for_deep_links() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            runtime.path().to_owned(),
            runtime.path().join("push.json"),
        ));
        let body = reqwest::get(format!("http://{address}/w/example/threads/1"))
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(body.contains("Web Client"));
        task.abort();
    }

    #[tokio::test]
    async fn service_worker_is_served_without_immutable_caching() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            runtime.path().to_owned(),
            runtime.path().join("push.json"),
        ));
        let response = reqwest::get(format!("http://{address}/service-worker.js"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(response.text().await.unwrap().contains("notificationclick"));
        task.abort();
    }

    #[tokio::test]
    async fn push_subscription_can_be_registered_and_removed() {
        let runtime = tempfile::tempdir().unwrap();
        let state_path = runtime.path().join("push.json");
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            runtime.path().to_owned(),
            state_path.clone(),
        ));
        let client = reqwest::Client::new();
        let origin = format!("http://{address}");
        let key = client
            .get(format!("{origin}/api/push/key"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert!(key["public_key"].as_str().unwrap().len() > 80);

        let secret = SecretKey::random(&mut OsRng);
        let subscription = json!({
            "endpoint": "https://push.example.test/subscription",
            "keys": {
                "auth": URL_SAFE_NO_PAD.encode([7_u8; 16]),
                "p256dh": URL_SAFE_NO_PAD.encode(
                    secret.public_key().to_encoded_point(false).as_bytes()
                ),
            },
        });
        let response = client
            .put(format!("{origin}/api/push/subscription"))
            .json(&subscription)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            fs::read_to_string(&state_path)
                .unwrap()
                .contains("push.example.test")
        );

        let response = client
            .delete(format!("{origin}/api/push/subscription"))
            .json(&json!({"endpoint": "https://push.example.test/subscription"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            !fs::read_to_string(&state_path)
                .unwrap()
                .contains("push.example.test")
        );
        task.abort();
    }

    #[tokio::test]
    async fn all_routes_require_the_exact_host() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            runtime.path().to_owned(),
            runtime.path().join("push.json"),
        ));
        let response = reqwest::Client::new()
            .get(format!("http://{address}/health"))
            .header(header::HOST, format!("localhost:{}", address.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        task.abort();
    }

    #[tokio::test]
    async fn missing_static_assets_do_not_fall_back_to_html() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(
            listener,
            runtime.path().to_owned(),
            runtime.path().join("push.json"),
        ));
        let response = reqwest::get(format!("http://{address}/assets/missing.js"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        task.abort();
    }

    #[tokio::test]
    async fn browser_api_discovers_a_controller_and_forwards_commands_and_snapshots() {
        let root = tempfile::tempdir().unwrap();
        let runtime = root.path().join("runtime");
        let workspace = root.path().join("workspace");
        let controller_directory = runtime.join("workspace-1");
        fs::create_dir_all(&controller_directory).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&controller_directory, fs::Permissions::from_mode(0o700)).unwrap();
        let metadata = json!({
            "workspace_id": "workspace-1",
            "path": workspace.canonicalize().unwrap(),
        });
        let metadata_path = controller_directory.join("workspace.json");
        fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        fs::set_permissions(&metadata_path, fs::Permissions::from_mode(0o600)).unwrap();

        let endpoint = controller_directory.join("controller.sock");
        let database = root.path().join("controller.sqlite3");
        let auth_home = root.path().join("auth");
        let data_home = root.path().join("data");
        let controller_endpoint = endpoint.clone();
        let controller = tokio::spawn(async move {
            atra_controller::run(
                &controller_endpoint,
                &database,
                &auth_home,
                &data_home,
                None,
            )
            .await
            .unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while !endpoint.exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();

        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let push_state = runtime.join("push.json");
        let server = tokio::spawn(serve(listener, runtime, push_state));
        let client = reqwest::Client::new();
        let origin = format!("http://{address}");

        let workspaces: serde_json::Value = client
            .get(format!("{origin}/api/workspaces"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(workspaces["workspaces"][0]["workspace_id"], "workspace-1");

        let result: CommandResult = client
            .post(format!("{origin}/api/workspaces/workspace-1/commands"))
            .header(header::ORIGIN, &origin)
            .json(&Command::ThreadCreate {
                display_name: Some("Web Thread".to_owned()),
            })
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(matches!(result, CommandResult::ThreadCreated { .. }));

        let response = client
            .get(format!(
                "{origin}/api/workspaces/workspace-1/controller/events"
            ))
            .send()
            .await
            .unwrap();
        let mut stream = response.bytes_stream();
        let mut body = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !body.windows(2).any(|bytes| bytes == b"\n\n") {
                body.extend_from_slice(&stream.next().await.unwrap().unwrap());
            }
        })
        .await
        .unwrap();
        let event = std::str::from_utf8(&body).unwrap();
        assert!(event.contains("\"message\":\"snapshot\""));
        assert!(event.contains("\"display_name\":\"Web Thread\""));

        server.abort();
        controller.abort();
    }
}
