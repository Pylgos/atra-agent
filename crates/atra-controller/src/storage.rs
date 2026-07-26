use std::path::Path;

use atra_protocol::{Thread, ThreadCheckpoint};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_rusqlite::{
    Connection, params,
    rusqlite::{self, types::Type},
};

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EventKind {
    WorkspaceInstructions,
    Skills,
    Runners,
    UserMessage,
    AssistantMessage,
    WebSearch,
    ToolCall,
    ToolResult,
    Reasoning,
    Compaction,
    ModelRequest,
    TokenUsage,
    RateLimits,
}

impl EventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::WorkspaceInstructions => "workspace_instructions",
            Self::Skills => "skills",
            Self::Runners => "runners",
            Self::UserMessage => "user_message",
            Self::AssistantMessage => "assistant_message",
            Self::WebSearch => "web_search",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::Reasoning => "reasoning",
            Self::Compaction => "compaction",
            Self::ModelRequest => "model_request",
            Self::TokenUsage => "token_usage",
            Self::RateLimits => "rate_limits",
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

                    CREATE TABLE IF NOT EXISTS checkpoints (
                        id INTEGER PRIMARY KEY,
                        thread_id INTEGER NOT NULL REFERENCES threads(id),
                        created_at_ms INTEGER NOT NULL,
                        reason TEXT NOT NULL,
                        display_name TEXT,
                        model TEXT NOT NULL,
                        reasoning_effort TEXT NOT NULL
                    );

                    CREATE TABLE IF NOT EXISTS checkpoint_events (
                        checkpoint_id INTEGER NOT NULL REFERENCES checkpoints(id),
                        sequence INTEGER NOT NULL,
                        kind TEXT NOT NULL,
                        payload TEXT NOT NULL,
                        PRIMARY KEY (checkpoint_id, sequence)
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

    pub async fn create_checkpoint(
        &self,
        thread_id: i64,
        created_at_ms: i64,
        reason: String,
    ) -> tokio_rusqlite::Result<i64> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let checkpoint_id =
                    create_checkpoint(&transaction, thread_id, created_at_ms, &reason)?;
                transaction.commit()?;
                Ok(checkpoint_id)
            })
            .await
    }

    pub async fn checkpoints(
        &self,
        thread_id: i64,
    ) -> tokio_rusqlite::Result<Vec<ThreadCheckpoint>> {
        self.connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "
                    SELECT id, thread_id, created_at_ms, reason
                    FROM checkpoints
                    WHERE thread_id = ?1
                    ORDER BY id DESC
                    ",
                )?;
                statement
                    .query_map([thread_id], |row| {
                        Ok(ThreadCheckpoint {
                            id: row.get(0)?,
                            thread_id: row.get(1)?,
                            created_at_ms: row.get(2)?,
                            reason: row.get(3)?,
                        })
                    })?
                    .collect()
            })
            .await
    }

    pub async fn checkpoint_events(
        &self,
        checkpoint_id: i64,
    ) -> tokio_rusqlite::Result<Vec<Event>> {
        self.connection
            .call(move |connection| {
                read_events(
                    connection,
                    "
                    SELECT sequence, kind, payload
                    FROM checkpoint_events
                    WHERE checkpoint_id = ?1
                    ORDER BY sequence
                    ",
                    checkpoint_id,
                )
            })
            .await
    }

    pub async fn fork_thread(
        &self,
        thread_id: i64,
        checkpoint_id: Option<i64>,
        sequence: i64,
        display_name: Option<String>,
    ) -> tokio_rusqlite::Result<i64> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let kind =
                    validate_history_point(&transaction, thread_id, checkpoint_id, sequence)?;
                let (source_name, model, reasoning_effort): (Option<String>, String, String) =
                    match checkpoint_id {
                        Some(checkpoint_id) => transaction.query_row(
                            "
                            SELECT display_name, model, reasoning_effort
                            FROM checkpoints
                            WHERE id = ?1 AND thread_id = ?2
                            ",
                            params![checkpoint_id, thread_id],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?,
                        None => transaction.query_row(
                            "
                            SELECT display_name, model, reasoning_effort
                            FROM threads
                            WHERE id = ?1
                            ",
                            [thread_id],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                        )?,
                    };
                transaction.execute(
                    "
                    INSERT INTO threads (display_name, model, reasoning_effort)
                    VALUES (?1, ?2, ?3)
                    ",
                    params![display_name.or(source_name), model, reasoning_effort],
                )?;
                let new_thread_id = transaction.last_insert_rowid();
                copy_events(
                    &transaction,
                    new_thread_id,
                    thread_id,
                    checkpoint_id,
                    Some(history_end_sequence(kind, sequence)),
                )?;
                transaction.commit()?;
                Ok(new_thread_id)
            })
            .await
    }

    pub async fn rewind(
        &self,
        thread_id: i64,
        checkpoint_id: Option<i64>,
        sequence: i64,
        created_at_ms: i64,
    ) -> tokio_rusqlite::Result<i64> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let kind =
                    validate_history_point(&transaction, thread_id, checkpoint_id, sequence)?;
                let saved = create_checkpoint(&transaction, thread_id, created_at_ms, "rewind")?;
                transaction.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id])?;
                if let Some(checkpoint_id) = checkpoint_id {
                    transaction.execute(
                        "
                        UPDATE threads
                        SET (display_name, model, reasoning_effort) = (
                            SELECT display_name, model, reasoning_effort
                            FROM checkpoints
                            WHERE id = ?1 AND thread_id = ?2
                        )
                        WHERE id = ?2
                        ",
                        params![checkpoint_id, thread_id],
                    )?;
                }
                copy_events(
                    &transaction,
                    thread_id,
                    thread_id,
                    Some(checkpoint_id.unwrap_or(saved)),
                    Some(history_end_sequence(kind, sequence)),
                )?;
                transaction.commit()?;
                Ok(saved)
            })
            .await
    }

    pub async fn restore_checkpoint(
        &self,
        thread_id: i64,
        checkpoint_id: i64,
        created_at_ms: i64,
    ) -> tokio_rusqlite::Result<i64> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                let (display_name, model, reasoning_effort): (Option<String>, String, String) =
                    transaction.query_row(
                        "
                        SELECT display_name, model, reasoning_effort
                        FROM checkpoints
                        WHERE id = ?1 AND thread_id = ?2
                        ",
                        params![checkpoint_id, thread_id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                let saved = create_checkpoint(&transaction, thread_id, created_at_ms, "restore")?;
                transaction.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id])?;
                transaction.execute(
                    "
                    UPDATE threads
                    SET display_name = ?1, model = ?2, reasoning_effort = ?3
                    WHERE id = ?4
                    ",
                    params![display_name, model, reasoning_effort, thread_id],
                )?;
                copy_events(
                    &transaction,
                    thread_id,
                    thread_id,
                    Some(checkpoint_id),
                    None,
                )?;
                transaction.commit()?;
                Ok(saved)
            })
            .await
    }

    pub async fn replace_with_compaction(
        &self,
        thread_id: i64,
        items: Value,
        workspace_instructions: Option<Value>,
        skills: Option<Value>,
        runners: Option<Value>,
    ) -> tokio_rusqlite::Result<()> {
        let items = serde_json::to_string(&items).map_err(to_sql_error)?;
        let workspace_instructions = workspace_instructions
            .map(|value| serde_json::to_string(&value).map_err(to_sql_error))
            .transpose()?;
        let skills = skills
            .map(|value| serde_json::to_string(&value).map_err(to_sql_error))
            .transpose()?;
        let runners = runners
            .map(|value| serde_json::to_string(&value).map_err(to_sql_error))
            .transpose()?;
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                transaction.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id])?;
                transaction.execute(
                    "
                    INSERT INTO events (thread_id, sequence, kind, payload)
                    VALUES (?1, 0, ?2, ?3)
                    ",
                    params![thread_id, EventKind::Compaction.as_str(), items],
                )?;
                if let Some(workspace_instructions) = workspace_instructions {
                    transaction.execute(
                        "
                        INSERT INTO events (thread_id, sequence, kind, payload)
                        VALUES (?1, 1, ?2, ?3)
                        ",
                        params![
                            thread_id,
                            EventKind::WorkspaceInstructions.as_str(),
                            workspace_instructions
                        ],
                    )?;
                }
                if let Some(skills) = skills {
                    transaction.execute(
                        "
                        INSERT INTO events (thread_id, sequence, kind, payload)
                        VALUES (
                            ?1,
                            COALESCE((SELECT MAX(sequence) + 1 FROM events WHERE thread_id = ?1), 0),
                            ?2,
                            ?3
                        )
                        ",
                        params![thread_id, EventKind::Skills.as_str(), skills],
                    )?;
                }
                if let Some(runners) = runners {
                    transaction.execute(
                        "
                        INSERT INTO events (thread_id, sequence, kind, payload)
                        VALUES (
                            ?1,
                            COALESCE((SELECT MAX(sequence) + 1 FROM events WHERE thread_id = ?1), 0),
                            ?2,
                            ?3
                        )
                        ",
                        params![thread_id, EventKind::Runners.as_str(), runners],
                    )?;
                }
                transaction.commit()
            })
            .await
    }
}

fn to_sql_error(error: serde_json::Error) -> tokio_rusqlite::Error {
    tokio_rusqlite::Error::Error(rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
}

fn create_checkpoint(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: i64,
    created_at_ms: i64,
    reason: &str,
) -> rusqlite::Result<i64> {
    let (display_name, model, reasoning_effort): (Option<String>, String, String) = transaction
        .query_row(
            "
            SELECT display_name, model, reasoning_effort
            FROM threads
            WHERE id = ?1
            ",
            [thread_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
    transaction.execute(
        "
        INSERT INTO checkpoints (
            thread_id, created_at_ms, reason, display_name, model, reasoning_effort
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![
            thread_id,
            created_at_ms,
            reason,
            display_name,
            model,
            reasoning_effort
        ],
    )?;
    let checkpoint_id = transaction.last_insert_rowid();
    transaction.execute(
        "
        INSERT INTO checkpoint_events (checkpoint_id, sequence, kind, payload)
        SELECT ?1, sequence, kind, payload
        FROM events
        WHERE thread_id = ?2
        ",
        params![checkpoint_id, thread_id],
    )?;
    Ok(checkpoint_id)
}

fn validate_history_point(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: i64,
    checkpoint_id: Option<i64>,
    sequence: i64,
) -> rusqlite::Result<EventKind> {
    let kind: String = match checkpoint_id {
        Some(checkpoint_id) => transaction.query_row(
            "
            SELECT ce.kind
            FROM checkpoint_events ce
            JOIN checkpoints c ON c.id = ce.checkpoint_id
            WHERE ce.checkpoint_id = ?1 AND c.thread_id = ?2 AND ce.sequence = ?3
            ",
            params![checkpoint_id, thread_id, sequence],
            |row| row.get(0),
        )?,
        None => transaction.query_row(
            "
            SELECT kind FROM events
            WHERE thread_id = ?1 AND sequence = ?2
            ",
            params![thread_id, sequence],
            |row| row.get(0),
        )?,
    };
    let kind = EventKind::try_from(kind.as_str())?;
    if !matches!(kind, EventKind::UserMessage | EventKind::AssistantMessage) {
        Err(rusqlite::Error::InvalidQuery)
    } else {
        Ok(kind)
    }
}

fn history_end_sequence(kind: EventKind, sequence: i64) -> i64 {
    match kind {
        EventKind::UserMessage => sequence - 1,
        EventKind::AssistantMessage => sequence,
        _ => unreachable!(),
    }
}

fn copy_events(
    transaction: &rusqlite::Transaction<'_>,
    destination_thread_id: i64,
    source_thread_id: i64,
    checkpoint_id: Option<i64>,
    through_sequence: Option<i64>,
) -> rusqlite::Result<()> {
    match (checkpoint_id, through_sequence) {
        (Some(checkpoint_id), Some(sequence)) => transaction.execute(
            "
            INSERT INTO events (thread_id, sequence, kind, payload)
            SELECT ?1, sequence, kind, payload
            FROM checkpoint_events
            WHERE checkpoint_id = ?2 AND sequence <= ?3
            ",
            params![destination_thread_id, checkpoint_id, sequence],
        )?,
        (Some(checkpoint_id), None) => transaction.execute(
            "
            INSERT INTO events (thread_id, sequence, kind, payload)
            SELECT ?1, sequence, kind, payload
            FROM checkpoint_events
            WHERE checkpoint_id = ?2
            ",
            params![destination_thread_id, checkpoint_id],
        )?,
        (None, Some(sequence)) => transaction.execute(
            "
            INSERT INTO events (thread_id, sequence, kind, payload)
            SELECT ?1, sequence, kind, payload
            FROM events
            WHERE thread_id = ?2 AND sequence <= ?3
            ",
            params![destination_thread_id, source_thread_id, sequence],
        )?,
        (None, None) => unreachable!(),
    };
    Ok(())
}

fn read_events(
    connection: &rusqlite::Connection,
    sql: &str,
    id: i64,
) -> rusqlite::Result<Vec<Event>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([id], |row| {
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
                rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
            })?,
        })
    })
    .collect()
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
