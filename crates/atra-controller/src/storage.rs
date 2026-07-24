use std::path::Path;

use atra_protocol::Thread;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rusqlite::{
    Connection, params,
    rusqlite::{self, types::Type},
};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    ApprovalRequest,
    ApprovalResponse,
    Reasoning,
    Compaction,
    TokenUsage,
}

impl EventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AssistantMessage => "assistant_message",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::ApprovalRequest => "approval_request",
            Self::ApprovalResponse => "approval_response",
            Self::Reasoning => "reasoning",
            Self::Compaction => "compaction",
            Self::TokenUsage => "token_usage",
        }
    }
}

impl TryFrom<&str> for EventKind {
    type Error = rusqlite::Error;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        serde_json::from_value(Value::String(value.to_owned())).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error))
        })
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub(crate) struct Event {
    pub sequence: i64,
    pub kind: EventKind,
    pub payload: Value,
}

pub(crate) struct Store {
    connection: Connection,
}

impl Store {
    pub async fn open(path: &Path) -> tokio_rusqlite::Result<Self> {
        let connection = Connection::open(path).await?;
        connection
            .call(|connection| {
                connection.execute_batch(
                    "
                    PRAGMA foreign_keys = ON;

                    CREATE TABLE IF NOT EXISTS threads (
                        id INTEGER PRIMARY KEY,
                        display_name TEXT,
                        model TEXT NOT NULL,
                        reasoning_effort TEXT NOT NULL
                    );

                    CREATE TABLE IF NOT EXISTS events (
                        thread_id INTEGER NOT NULL REFERENCES threads(id),
                        sequence INTEGER NOT NULL,
                        kind TEXT NOT NULL,
                        payload TEXT NOT NULL,
                        PRIMARY KEY (thread_id, sequence)
                    );
                    ",
                )?;
                Ok(())
            })
            .await?;
        Ok(Self { connection })
    }

    pub async fn create_thread(
        &self,
        display_name: Option<String>,
        model: String,
        reasoning_effort: String,
    ) -> tokio_rusqlite::Result<i64> {
        self.connection
            .call(move |connection| {
                connection.execute(
                    "INSERT INTO threads (display_name, model, reasoning_effort) VALUES (?1, ?2, ?3)",
                    params![display_name, model, reasoning_effort],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .await
    }

    pub async fn threads(&self) -> tokio_rusqlite::Result<Vec<Thread>> {
        self.connection
            .call(|connection| {
                let mut statement = connection
                    .prepare("SELECT id, display_name, model, reasoning_effort FROM threads ORDER BY id DESC")?;
                statement
                    .query_map([], |row| {
                        Ok(Thread {
                            id: row.get(0)?,
                            display_name: row.get(1)?,
                            model: row.get(2)?,
                            reasoning_effort: row.get(3)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
    }

    pub async fn rename_thread(
        &self,
        thread_id: i64,
        display_name: String,
    ) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                let updated = connection.execute(
                    "UPDATE threads SET display_name = ?1 WHERE id = ?2",
                    params![display_name, thread_id],
                )?;
                if updated == 0 {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                Ok(())
            })
            .await
    }

    pub async fn set_thread_model(
        &self,
        thread_id: i64,
        model: String,
        reasoning_effort: String,
    ) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                let updated = connection.execute(
                    "UPDATE threads SET model = ?1, reasoning_effort = ?2 WHERE id = ?3",
                    params![model, reasoning_effort, thread_id],
                )?;
                if updated == 0 {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                Ok(())
            })
            .await
    }

    pub async fn thread_model(&self, thread_id: i64) -> tokio_rusqlite::Result<(String, String)> {
        self.connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT model, reasoning_effort FROM threads WHERE id = ?1",
                    [thread_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
            })
            .await
    }

    pub async fn name_thread_if_unnamed(
        &self,
        thread_id: i64,
        display_name: String,
    ) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                connection.execute(
                    "
                    UPDATE threads
                    SET display_name = ?1
                    WHERE id = ?2 AND display_name IS NULL
                    ",
                    params![display_name, thread_id],
                )?;
                Ok(())
            })
            .await
    }

    pub async fn append(
        &self,
        thread_id: i64,
        kind: EventKind,
        payload: Value,
    ) -> tokio_rusqlite::Result<i64> {
        let payload = serde_json::to_string(&payload).map_err(|error| {
            tokio_rusqlite::Error::Error(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
        })?;
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let sequence = transaction.query_row(
                    "
                    SELECT COALESCE(MAX(sequence) + 1, 0)
                    FROM events
                    WHERE thread_id = ?1
                    ",
                    [thread_id],
                    |row| row.get(0),
                )?;
                transaction.execute(
                    "
                    INSERT INTO events (thread_id, sequence, kind, payload)
                    VALUES (?1, ?2, ?3, ?4)
                    ",
                    params![thread_id, sequence, kind.as_str(), payload],
                )?;
                transaction.commit()?;
                Ok(sequence)
            })
            .await
    }

    pub async fn events(&self, thread_id: i64) -> tokio_rusqlite::Result<Vec<Event>> {
        self.connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "
                    SELECT sequence, kind, payload
                    FROM events
                    WHERE thread_id = ?1
                    ORDER BY sequence
                    ",
                )?;
                let rows = statement.query_map([thread_id], |row| {
                    let kind: String = row.get(1)?;
                    let payload: String = row.get(2)?;
                    Ok((row.get(0)?, kind, payload))
                })?;

                rows.map(|row| {
                    let (sequence, kind, payload) = row?;
                    Ok(Event {
                        sequence,
                        kind: EventKind::try_from(kind.as_str())?,
                        payload: serde_json::from_str(&payload).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                Type::Text,
                                Box::new(error),
                            )
                        })?,
                    })
                })
                .collect()
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn events_keep_their_thread_order() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let thread = store
            .create_thread(None, "test-model".to_owned(), "medium".to_owned())
            .await
            .unwrap();

        assert_eq!(
            store
                .append(thread, EventKind::UserMessage, json!({"content": "one"}))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .append(
                    thread,
                    EventKind::AssistantMessage,
                    json!({"content": "two"})
                )
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store.events(thread).await.unwrap(),
            vec![
                Event {
                    sequence: 0,
                    kind: EventKind::UserMessage,
                    payload: json!({"content": "one"}),
                },
                Event {
                    sequence: 1,
                    kind: EventKind::AssistantMessage,
                    payload: json!({"content": "two"}),
                },
            ]
        );
    }

    #[tokio::test]
    async fn events_survive_reopening_the_database() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("controller.sqlite3");
        let store = Store::open(&database).await.unwrap();
        let thread = store
            .create_thread(None, "test-model".to_owned(), "medium".to_owned())
            .await
            .unwrap();
        store
            .append(thread, EventKind::UserMessage, json!({"content": "saved"}))
            .await
            .unwrap();
        drop(store);

        let reopened = Store::open(&database).await.unwrap();

        assert_eq!(
            reopened.events(thread).await.unwrap(),
            vec![Event {
                sequence: 0,
                kind: EventKind::UserMessage,
                payload: json!({"content": "saved"}),
            }]
        );
    }

    #[tokio::test]
    async fn threads_are_listed_newest_first() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let first = store
            .create_thread(
                Some("First".to_owned()),
                "model-a".to_owned(),
                "low".to_owned(),
            )
            .await
            .unwrap();
        let second = store
            .create_thread(None, "model-b".to_owned(), "high".to_owned())
            .await
            .unwrap();

        assert_eq!(
            store.threads().await.unwrap(),
            vec![
                Thread {
                    id: second,
                    display_name: None,
                    model: "model-b".to_owned(),
                    reasoning_effort: "high".to_owned(),
                },
                Thread {
                    id: first,
                    display_name: Some("First".to_owned()),
                    model: "model-a".to_owned(),
                    reasoning_effort: "low".to_owned(),
                },
            ]
        );
    }
}
