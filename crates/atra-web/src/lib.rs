use std::{
    convert::Infallible,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use atra_client::Client;
use atra_protocol::{
    CheckpointId, CheckpointSubscriptionMessage, Command, ControllerSubscriptionMessage, ProcessId,
    ProcessLocator, ProcessSubscriptionMessage, SubscriptionTerminal, ThreadId,
    ThreadSubscriptionMessage,
};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures_util::Stream;
use rustix::process::getuid;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/assets.rs"));
}

#[derive(Clone)]
struct AppState {
    runtime: PathBuf,
    authority: String,
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
        Self { status, message: message.into() }
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

fn router(runtime: PathBuf, port: u16) -> Router {
    let state = AppState {
        runtime,
        authority: format!("127.0.0.1:{port}"),
    };
    Router::new()
        .route("/health", get(|| async { Json(json!({"service": "atra-web"})) }))
        .route("/api/workspaces", get(workspaces))
        .route("/api/workspaces/events", get(workspace_events))
        .route("/api/workspaces/{workspace}/controller/events", get(controller_events))
        .route("/api/workspaces/{workspace}/threads/{thread}/events", get(thread_events))
        .route("/api/workspaces/{workspace}/threads/{thread}/checkpoints/{checkpoint}/events", get(checkpoint_events))
        .route("/api/workspaces/{workspace}/runners/{runner}/processes/{process}/events", get(process_events))
        .route("/api/workspaces/{workspace}/commands", post(commands))
        .fallback(get(asset))
        .layer(DefaultBodyLimit::max(64 * 1024))
        .with_state(state)
}


async fn asset(uri: axum::http::Uri) -> Response {
    let requested = uri.path();
    let normalized = requested.replace("/./", "/");
    let asset_path = if normalized == "/" { "/index.html" } else { normalized.as_str() };
    if let Some((bytes, content_type)) = embedded::get(asset_path) {
        return ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
    }
    if let Some((bytes, content_type)) = embedded::get("/index.html") {
        return ([(header::CONTENT_TYPE, content_type)], bytes).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

pub async fn serve(listener: TcpListener, runtime: PathBuf) -> Result<()> {
    let port = listener.local_addr()?.port();
    axum::serve(listener, router(runtime, port))
        .await
        .context("Web daemon failed")
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
    let mut subscription = client.subscribe_controller().await.map_err(ApiError::unavailable)?;
    let snapshot = ControllerSubscriptionMessage::Snapshot { state: subscription.state().clone() };
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
    let mut subscription = client.subscribe_thread(ThreadId(thread)).await.map_err(ApiError::unavailable)?;
    let snapshot = ThreadSubscriptionMessage::Snapshot { state: subscription.state().clone() };
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
    let mut subscription = client.subscribe_checkpoint(CheckpointId(checkpoint)).await.map_err(ApiError::unavailable)?;
    let snapshot = CheckpointSubscriptionMessage::Snapshot { state: subscription.state().clone() };
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
struct ProcessQuery { thread_id: i64 }

async fn process_events(
    State(state): State<AppState>,
    AxumPath((workspace, runner, process)): AxumPath<(String, String, String)>,
    Query(query): Query<ProcessQuery>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, ApiError> {
    let client = workspace_client(&state.runtime, &workspace).await?;
    let locator = ProcessLocator::new(ThreadId(query.thread_id), runner, ProcessId(process));
    let mut subscription = client.subscribe_process(locator).await.map_err(ApiError::unavailable)?;
    let snapshot = ProcessSubscriptionMessage::Snapshot { state: subscription.state().clone() };
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
    let result = client.command(command).await.map_err(|error| {
        ApiError::new(StatusCode::CONFLICT, format!("{error:#}"))
    })?;
    serde_json::to_value(result)
        .map(Json)
        .map_err(|error| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

fn check_command_headers(authority: &str, headers: &HeaderMap) -> Result<(), ApiError> {
    let host = headers.get(header::HOST).and_then(|value| value.to_str().ok());
    if host != Some(authority) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Host does not match the Web daemon origin"));
    }
    let expected_origin = format!("http://{authority}");
    let origin = headers.get(header::ORIGIN).and_then(|value| value.to_str().ok());
    if origin != Some(expected_origin.as_str()) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "Origin does not match the Web daemon origin"));
    }
    let content_type = headers.get(header::CONTENT_TYPE).and_then(|value| value.to_str().ok());
    if !content_type.is_some_and(|value| value.eq_ignore_ascii_case("application/json") || value.to_ascii_lowercase().starts_with("application/json;")) {
        return Err(ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, "commands require application/json"));
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
    let Ok(entries) = fs::read_dir(runtime) else { return Vec::new() };
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
    if !private(&directory, 0o700, true) { return None; }
    let metadata_path = directory.join("workspace.json");
    if !private(&metadata_path, 0o600, false) { return None; }
    let metadata: WorkspaceMetadata = serde_json::from_slice(&fs::read(metadata_path).ok()?).ok()?;
    if directory.file_name()?.to_str()? != metadata.workspace_id { return None; }
    let canonical = fs::canonicalize(&metadata.path).ok()?;
    if canonical != metadata.path { return None; }
    let endpoint = directory.join("controller.sock");
    Client::new(&endpoint).subscribe_controller().await.ok()?;
    let name = canonical.file_name()?.to_string_lossy().into_owned();
    Some(Workspace { workspace_id: metadata.workspace_id, name, path: canonical, endpoint })
}

fn private(path: &Path, mode: u32, directory: bool) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else { return false };
    !metadata.file_type().is_symlink()
        && if directory { metadata.is_dir() } else { metadata.is_file() }
        && metadata.uid() == getuid().as_raw()
        && metadata.mode() & 0o777 == mode
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_security_requires_exact_same_origin() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:2872".parse().unwrap());
        headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        assert!(check_command_headers("127.0.0.1:2872", &headers).is_err());
        headers.insert(header::ORIGIN, "http://127.0.0.1:2872".parse().unwrap());
        assert!(check_command_headers("127.0.0.1:2872", &headers).is_ok());
        headers.insert(header::ORIGIN, "http://localhost:2872".parse().unwrap());
        assert!(check_command_headers("127.0.0.1:2872", &headers).is_err());
    }

    #[tokio::test]
    async fn command_endpoint_rejects_cross_origin_requests() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(listener, runtime.path().to_owned()));
        let response = reqwest::Client::new()
            .post(format!("http://{address}/api/workspaces/missing/commands"))
            .header("origin", "http://attacker.invalid")
            .json(&Command::Shutdown)
            .send().await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        task.abort();
    }

    #[tokio::test]
    async fn embedded_application_is_served_for_deep_links() {
        let runtime = tempfile::tempdir().unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(serve(listener, runtime.path().to_owned()));
        let body = reqwest::get(format!("http://{address}/w/example/threads/1"))
            .await.unwrap().text().await.unwrap();
        assert!(body.contains("Web Client"));
        task.abort();
    }
}
