use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ControllerRequest {
    Status,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ControllerResponse {
    Running,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum RunnerRequest {
    Initialize,
}

#[derive(Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum RunnerResponse {
    Ready,
}
