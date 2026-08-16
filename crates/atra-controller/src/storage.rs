use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use atra_protocol::{
    CheckpointId, CompactionEvent, EventSequence, FrozenBoundaryEvent, HistoryTarget,
    InstructionEvent, MessageEvent, RunnersEvent, Thread, ThreadCheckpoint, ThreadEventData,
    ThreadId,
};
use serde_json::Value;
use tokio_rusqlite::{
    Connection, params,
    rusqlite::{self, types::Type},
};

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct Event {
    pub sequence: EventSequence,
    #[serde(flatten)]
    pub data: ThreadEventData,
}

pub(crate) struct ReportSnapshot {
    pub(crate) through: EventSequence,
    pub(crate) events: Vec<Event>,
}

pub(crate) struct FrozenBoundary {
    pub through_sequence: EventSequence,
    pub masked_sequences: Vec<EventSequence>,
}

pub(crate) fn latest_frozen_boundary(events: &[Event]) -> Option<FrozenBoundary> {
    for event in events.iter().rev() {
        match &event.data {
            ThreadEventData::FrozenBoundary(boundary) => {
                return Some(FrozenBoundary {
                    through_sequence: boundary.through_sequence,
                    masked_sequences: boundary.masked_sequences.clone(),
                });
            }
            ThreadEventData::Compaction(_) => return None,
            _ => {}
        }
    }
    None
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
                        id INTEGER PRIMARY KEY AUTOINCREMENT,
                        parent_thread_id INTEGER REFERENCES threads(id),
                        display_name TEXT,
                        provider TEXT NOT NULL,
                        model TEXT NOT NULL,
                        reasoning_effort TEXT NOT NULL,
                        allow_delegation INTEGER NOT NULL DEFAULT 0
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
                        provider TEXT NOT NULL,
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
        provider: String,
        model: String,
        reasoning_effort: String,
    ) -> tokio_rusqlite::Result<ThreadId> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                transaction.execute(
                    "INSERT INTO threads (display_name, provider, model, reasoning_effort, allow_delegation) VALUES (?1, ?2, ?3, ?4, 0)",
                    params![display_name, provider, model, reasoning_effort],
                )?;
                let thread_id = transaction.last_insert_rowid();
                insert_thread_context(&transaction, thread_id, false)?;
                transaction.commit()?;
                Ok(ThreadId(thread_id))
            })
            .await
    }

    pub async fn create_child_thread(
        &self,
        parent_thread_id: ThreadId,
        display_name: String,
        provider: String,
        model: String,
        reasoning_effort: String,
        allow_delegation: bool,
    ) -> tokio_rusqlite::Result<ThreadId> {
        self.connection.call(move |connection| {
            let transaction = connection.transaction()?;
            transaction.execute(
                "INSERT INTO threads (parent_thread_id, display_name, provider, model, reasoning_effort, allow_delegation) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![parent_thread_id.0, display_name, provider, model, reasoning_effort, allow_delegation],
            )?;
            let thread_id = transaction.last_insert_rowid();
            insert_thread_context(&transaction, thread_id, allow_delegation)?;
            transaction.commit()?;
            Ok(ThreadId(thread_id))
        }).await
    }

    pub async fn delegation_allowed(&self, thread_id: ThreadId) -> tokio_rusqlite::Result<bool> {
        self.connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT allow_delegation FROM threads WHERE id = ?1",
                    [thread_id.0],
                    |row| row.get(0),
                )
            })
            .await
    }

    pub async fn threads(&self) -> tokio_rusqlite::Result<Vec<Thread>> {
        self.connection
            .call(|connection| {
                let mut statement = connection
                    .prepare("SELECT id, parent_thread_id, display_name, provider, model, reasoning_effort FROM threads ORDER BY id DESC")?;
                statement
                    .query_map([], |row| {
                        Ok(Thread {
                            id: ThreadId(row.get(0)?),
                            parent_thread_id: row.get::<_, Option<i64>>(1)?.map(ThreadId),
                            display_name: row.get(2)?,
                            provider: row.get(3)?,
                            model: row.get(4)?,
                            reasoning_effort: row.get(5)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()
            })
            .await
    }

    pub async fn thread(&self, thread_id: ThreadId) -> tokio_rusqlite::Result<Thread> {
        self.connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT id, parent_thread_id, display_name, provider, model, reasoning_effort FROM threads WHERE id = ?1",
                    [thread_id.0],
                    |row| {
                        Ok(Thread {
                            id: ThreadId(row.get(0)?),
                            parent_thread_id: row.get::<_, Option<i64>>(1)?.map(ThreadId),
                            display_name: row.get(2)?,
                            provider: row.get(3)?,
                            model: row.get(4)?,
                            reasoning_effort: row.get(5)?,
                        })
                    },
                )
            })
            .await
    }

    pub async fn descendants(&self, thread_id: ThreadId) -> tokio_rusqlite::Result<Vec<ThreadId>> {
        self.connection.call(move |connection| {
            let mut statement = connection.prepare(
                "WITH RECURSIVE descendants(id) AS (SELECT id FROM threads WHERE parent_thread_id = ?1 UNION ALL SELECT t.id FROM threads t JOIN descendants d ON t.parent_thread_id = d.id) SELECT id FROM descendants ORDER BY id"
            )?;
            statement.query_map([thread_id.0], |row| Ok(ThreadId(row.get(0)?)))?
                .collect::<Result<Vec<_>, _>>()
        }).await
    }

    pub async fn is_descendant(
        &self,
        ancestor: ThreadId,
        candidate: ThreadId,
    ) -> tokio_rusqlite::Result<bool> {
        Ok(self.descendants(ancestor).await?.contains(&candidate))
    }

    pub async fn root_thread(&self, thread_id: ThreadId) -> tokio_rusqlite::Result<ThreadId> {
        self.connection.call(move |connection| connection.query_row(
            "WITH RECURSIVE ancestry(id, parent_thread_id) AS (SELECT id, parent_thread_id FROM threads WHERE id = ?1 UNION ALL SELECT t.id, t.parent_thread_id FROM threads t JOIN ancestry a ON a.parent_thread_id = t.id) SELECT id FROM ancestry WHERE parent_thread_id IS NULL",
            [thread_id.0], |row| Ok(ThreadId(row.get(0)?))
        )).await
    }

    pub async fn rename_thread(
        &self,
        thread_id: ThreadId,
        display_name: String,
    ) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
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

    pub async fn delete_threads(&self, thread_ids: Vec<ThreadId>) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                let transaction = connection.transaction()?;
                for thread_id in thread_ids {
                    transaction.execute(
                        "DELETE FROM checkpoint_events WHERE checkpoint_id IN (SELECT id FROM checkpoints WHERE thread_id = ?1)",
                        [thread_id.0],
                    )?;
                    transaction.execute("DELETE FROM checkpoints WHERE thread_id = ?1", [thread_id.0])?;
                    transaction.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id.0])?;
                    let updated = transaction.execute("DELETE FROM threads WHERE id = ?1", [thread_id.0])?;
                    if updated == 0 { return Err(rusqlite::Error::QueryReturnedNoRows); }
                }
                transaction.commit()?;
                Ok(())
            })
            .await
    }

    pub async fn set_thread_model(
        &self,
        thread_id: ThreadId,
        provider: String,
        model: String,
        reasoning_effort: String,
    ) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                let updated = connection.execute(
                    "UPDATE threads SET provider = ?1, model = ?2, reasoning_effort = ?3 WHERE id = ?4",
                    params![provider, model, reasoning_effort, thread_id],
                )?;
                if updated == 0 {
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                }
                Ok(())
            })
            .await
    }

    pub async fn thread_model(
        &self,
        thread_id: ThreadId,
    ) -> tokio_rusqlite::Result<(String, String, String)> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                connection.query_row(
                    "SELECT provider, model, reasoning_effort FROM threads WHERE id = ?1",
                    [thread_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
            })
            .await
    }

    pub async fn name_thread_if_unnamed(
        &self,
        thread_id: ThreadId,
        display_name: String,
    ) -> tokio_rusqlite::Result<()> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
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
        thread_id: ThreadId,
        data: ThreadEventData,
    ) -> tokio_rusqlite::Result<EventSequence> {
        Ok(self.append_all(thread_id, vec![data]).await?[0])
    }

    pub async fn append_all(
        &self,
        thread_id: ThreadId,
        events: Vec<ThreadEventData>,
    ) -> tokio_rusqlite::Result<Vec<EventSequence>> {
        let events = events
            .into_iter()
            .map(|data| {
                let (kind, payload) = event_columns(&data)?;
                let payload = serde_json::to_string(&payload).map_err(|error| {
                    tokio_rusqlite::Error::Error(rusqlite::Error::ToSqlConversionFailure(Box::new(
                        error,
                    )))
                })?;
                Ok((kind, payload))
            })
            .collect::<tokio_rusqlite::Result<Vec<_>>>()?;
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                let transaction = connection.transaction()?;
                let first_sequence: i64 = transaction.query_row(
                    "
                    SELECT COALESCE(MAX(sequence) + 1, 0)
                    FROM events
                    WHERE thread_id = ?1
                    ",
                    [thread_id],
                    |row| row.get(0),
                )?;
                let mut sequences = Vec::with_capacity(events.len());
                for (offset, (kind, payload)) in events.into_iter().enumerate() {
                    let sequence = first_sequence + offset as i64;
                    transaction.execute(
                        "
                        INSERT INTO events (thread_id, sequence, kind, payload)
                        VALUES (?1, ?2, ?3, ?4)
                        ",
                        params![thread_id, sequence, kind.as_str(), payload],
                    )?;
                    sequences.push(EventSequence(sequence));
                }
                transaction.commit()?;
                Ok(sequences)
            })
            .await
    }

    pub async fn events(&self, thread_id: ThreadId) -> tokio_rusqlite::Result<Vec<Event>> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
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
                        sequence: EventSequence(sequence),
                        data: event_data(&kind, &payload)?,
                    })
                })
                .collect()
            })
            .await
    }

    pub async fn report_snapshot(
        &self,
        thread_id: ThreadId,
    ) -> tokio_rusqlite::Result<ReportSnapshot> {
        self.connection.call(move |connection| {
            let transaction = connection.transaction()?;
            let through = EventSequence(transaction.query_row(
                "SELECT COALESCE(MAX(sequence), -1) FROM events WHERE thread_id = ?1",
                [thread_id.0],
                |row| row.get(0),
            )?);
            let events = read_event_rows(
                &transaction,
                "SELECT sequence, kind, payload FROM events WHERE thread_id = ?1 AND sequence <= ?2 ORDER BY sequence",
                params![thread_id.0, through.0],
            )?;
            let mut output = Vec::new();
            reconstruct_events(
                &transaction,
                thread_id.0,
                events,
                &mut output,
                &mut HashSet::new(),
            )?;
            transaction.commit()?;
            Ok(ReportSnapshot { through, events: output })
        }).await
    }

    pub async fn active_tool_events(
        &self,
        thread_id: ThreadId,
    ) -> tokio_rusqlite::Result<Vec<Event>> {
        self.connection
            .call(move |connection| {
                read_events(
                    connection,
                    "
                    SELECT sequence, kind, payload
                    FROM events
                    WHERE thread_id = ?1
                        AND sequence > COALESCE(
                            (
                                SELECT MAX(sequence)
                                FROM events
                                WHERE thread_id = ?1 AND kind = 'compaction'
                            ),
                            -1
                        )
                        AND kind IN ('tool_call', 'tool_result')
                    ORDER BY sequence
                    ",
                    thread_id.0,
                )
            })
            .await
    }

    pub async fn latest_event(
        &self,
        thread_id: ThreadId,
        kind: &'static str,
    ) -> tokio_rusqlite::Result<Option<Event>> {
        self.connection
            .call(move |connection| {
                let mut statement = connection.prepare(
                    "
                    SELECT sequence, payload
                    FROM events
                    WHERE thread_id = ?1 AND kind = ?2
                    ORDER BY sequence DESC
                    LIMIT 1
                    ",
                )?;
                let mut rows = statement.query(params![thread_id.0, kind])?;
                let Some(row) = rows.next()? else {
                    return Ok(None);
                };
                let sequence = EventSequence(row.get(0)?);
                let payload: String = row.get(1)?;
                Ok(Some(Event {
                    sequence,
                    data: event_data(kind, &payload)?,
                }))
            })
            .await
    }

    pub async fn freeze_event_payloads(
        &self,
        thread_id: ThreadId,
        events: Vec<(EventSequence, ThreadEventData)>,
        boundary: FrozenBoundaryEvent,
    ) -> tokio_rusqlite::Result<EventSequence> {
        let events = events
            .into_iter()
            .map(|(sequence, data)| {
                let (_, payload) = event_columns(&data)?;
                serde_json::to_string(&payload)
                    .map(|payload| (sequence.0, payload))
                    .map_err(to_sql_error)
            })
            .collect::<tokio_rusqlite::Result<Vec<_>>>()?;
        let boundary = serde_json::to_string(&boundary).map_err(to_sql_error)?;
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                let transaction = connection.transaction()?;
                for (sequence, payload) in events {
                    transaction.execute(
                        "
                        UPDATE events
                        SET payload = ?1
                        WHERE thread_id = ?2 AND sequence = ?3
                        ",
                        params![payload, thread_id, sequence],
                    )?;
                }
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
                    params![thread_id, sequence, "frozen_boundary", boundary],
                )?;
                transaction.commit()?;
                Ok(EventSequence(sequence))
            })
            .await
    }

    pub async fn create_checkpoint(
        &self,
        thread_id: ThreadId,
        created_at_ms: i64,
        reason: String,
    ) -> tokio_rusqlite::Result<CheckpointId> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                let transaction = connection.transaction()?;
                let checkpoint_id =
                    create_checkpoint(&transaction, thread_id, created_at_ms, &reason)?;
                transaction.commit()?;
                Ok(CheckpointId(checkpoint_id))
            })
            .await
    }

    pub async fn checkpoints(
        &self,
        thread_id: ThreadId,
    ) -> tokio_rusqlite::Result<Vec<ThreadCheckpoint>> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
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
                            id: CheckpointId(row.get(0)?),
                            thread_id: ThreadId(row.get(1)?),
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
        checkpoint_id: CheckpointId,
    ) -> tokio_rusqlite::Result<Vec<Event>> {
        self.connection
            .call(move |connection| {
                let checkpoint_id = checkpoint_id.0;
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

    pub async fn checkpoint(
        &self,
        checkpoint_id: CheckpointId,
    ) -> tokio_rusqlite::Result<ThreadCheckpoint> {
        self.connection
            .call(move |connection| {
                connection.query_row(
                    "SELECT id, thread_id, created_at_ms, reason FROM checkpoints WHERE id = ?1",
                    [checkpoint_id.0],
                    |row| {
                        Ok(ThreadCheckpoint {
                            id: CheckpointId(row.get(0)?),
                            thread_id: ThreadId(row.get(1)?),
                            created_at_ms: row.get(2)?,
                            reason: row.get(3)?,
                        })
                    },
                )
            })
            .await
    }

    pub async fn fork_thread(
        &self,
        thread_id: ThreadId,
        checkpoint_id: Option<CheckpointId>,
        sequence: EventSequence,
        display_name: Option<String>,
    ) -> tokio_rusqlite::Result<ThreadId> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                let checkpoint_id = checkpoint_id.map(|id| id.0);
                let sequence = sequence.0;
                let transaction = connection.transaction()?;
                let kind =
                    validate_history_point(&transaction, thread_id, checkpoint_id, sequence)?;
                let (
                    source_parent,
                    source_name,
                    provider,
                    model,
                    reasoning_effort,
                    allow_delegation,
                ): (
                    Option<i64>,
                    Option<String>,
                    String,
                    String,
                    String,
                    bool,
                ) = match checkpoint_id {
                    Some(checkpoint_id) => transaction.query_row(
                        "
                            SELECT t.parent_thread_id, c.display_name, c.provider, c.model, c.reasoning_effort, t.allow_delegation
                            FROM checkpoints
                            c JOIN threads t ON t.id = c.thread_id
                            WHERE c.id = ?1 AND c.thread_id = ?2
                            ",
                        params![checkpoint_id, thread_id],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                            ))
                        },
                    )?,
                    None => transaction.query_row(
                        "
                            SELECT parent_thread_id, display_name, provider, model, reasoning_effort, allow_delegation
                            FROM threads
                            WHERE id = ?1
                            ",
                        [thread_id],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                                row.get(5)?,
                            ))
                        },
                    )?,
                };
                transaction.execute(
                    "
                    INSERT INTO threads (parent_thread_id, display_name, provider, model, reasoning_effort, allow_delegation)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                    ",
                    params![
                        source_parent,
                        display_name.or(source_name),
                        provider,
                        model,
                        reasoning_effort,
                        allow_delegation
                    ],
                )?;
                let new_thread_id = transaction.last_insert_rowid();
                copy_events(
                    &transaction,
                    new_thread_id,
                    thread_id,
                    checkpoint_id,
                    Some(history_end_sequence(kind, sequence)),
                )?;
                clone_compaction_checkpoints(&transaction, thread_id, new_thread_id)?;
                transaction.commit()?;
                Ok(ThreadId(new_thread_id))
            })
            .await
    }

    pub async fn replace_history(
        &self,
        thread_id: ThreadId,
        target: HistoryTarget,
        created_at_ms: i64,
    ) -> tokio_rusqlite::Result<CheckpointId> {
        self.connection
            .call(move |connection| {
                let thread_id = thread_id.0;
                let transaction = connection.transaction()?;
                let (checkpoint_id, through_sequence, reason) = match target {
                    HistoryTarget::Message {
                        checkpoint_id,
                        sequence,
                    } => {
                        let checkpoint_id = checkpoint_id.map(|id| id.0);
                        let kind = validate_history_point(
                            &transaction,
                            thread_id,
                            checkpoint_id,
                            sequence.0,
                        )?;
                        (
                            checkpoint_id,
                            Some(history_end_sequence(kind, sequence.0)),
                            "rewind",
                        )
                    }
                    HistoryTarget::Checkpoint { checkpoint_id } => {
                        (Some(checkpoint_id.0), None, "restore")
                    }
                };
                let checkpoint_settings: Option<(Option<String>, String, String, String)> =
                    checkpoint_id
                        .map(|checkpoint_id| {
                            transaction.query_row(
                                "
                            SELECT display_name, provider, model, reasoning_effort
                            FROM checkpoints
                            WHERE id = ?1 AND thread_id = ?2
                            ",
                                params![checkpoint_id, thread_id],
                                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                            )
                        })
                        .transpose()?;
                let saved = create_checkpoint(&transaction, thread_id, created_at_ms, reason)?;
                transaction.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id])?;
                if let Some((display_name, provider, model, reasoning_effort)) = checkpoint_settings
                {
                    transaction.execute(
                        "
                        UPDATE threads
                        SET display_name = ?1, provider = ?2, model = ?3, reasoning_effort = ?4
                        WHERE id = ?5
                        ",
                        params![display_name, provider, model, reasoning_effort, thread_id],
                    )?;
                }
                copy_events(
                    &transaction,
                    thread_id,
                    thread_id,
                    Some(checkpoint_id.unwrap_or(saved)),
                    through_sequence,
                )?;
                transaction.commit()?;
                Ok(CheckpointId(saved))
            })
            .await
    }

    pub async fn replace_with_compaction(
        &self,
        thread_id: ThreadId,
        compaction: CompactionEvent,
        workspace_instructions: Option<InstructionEvent>,
        skills: Option<InstructionEvent>,
        runners: Option<RunnersEvent>,
    ) -> tokio_rusqlite::Result<()> {
        let items = serde_json::to_string(&compaction).map_err(to_sql_error)?;
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
                let thread_id = thread_id.0;
                let transaction = connection.transaction()?;
                let thread_context: String = transaction.query_row(
                    "SELECT payload FROM events WHERE thread_id = ?1 AND kind = 'thread_context'",
                    [thread_id],
                    |row| row.get(0),
                )?;
                let next_sequence: i64 = transaction.query_row(
                    "SELECT COALESCE(MAX(sequence) + 1, 0) FROM events WHERE thread_id = ?1",
                    [thread_id], |row| row.get(0),
                )?;
                transaction.execute("DELETE FROM events WHERE thread_id = ?1", [thread_id])?;
                transaction.execute(
                    "
                    INSERT INTO events (thread_id, sequence, kind, payload)
                    VALUES (?1, 0, 'thread_context', ?2)
                    ",
                    params![thread_id, thread_context],
                )?;
                transaction.execute(
                    "
                    INSERT INTO events (thread_id, sequence, kind, payload)
                    VALUES (?1, ?2, ?3, ?4)
                    ",
                    params![thread_id, next_sequence, "compaction", items],
                )?;
                if let Some(workspace_instructions) = workspace_instructions {
                    transaction.execute(
                        "
                        INSERT INTO events (thread_id, sequence, kind, payload)
                        VALUES (?1, ?2, ?3, ?4)
                        ",
                        params![
                            thread_id,
                            next_sequence + 1,
                            "workspace_instructions",
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
                        params![thread_id, "skills", skills],
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
                        params![thread_id, "runners", runners],
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

fn insert_thread_context(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: i64,
    allow_delegation: bool,
) -> rusqlite::Result<()> {
    let mut statement = transaction.prepare(
        "
        WITH RECURSIVE ancestry(id, parent_thread_id, display_name, depth) AS (
            SELECT id, parent_thread_id, display_name, 0
            FROM threads
            WHERE id = ?1
            UNION ALL
            SELECT t.id, t.parent_thread_id, t.display_name, a.depth + 1
            FROM threads t
            JOIN ancestry a ON a.parent_thread_id = t.id
        )
        SELECT id, display_name
        FROM ancestry
        ORDER BY depth DESC
        ",
    )?;
    let rows = statement
        .query_map([thread_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let position = rows
        .iter()
        .enumerate()
        .map(|(index, (id, name))| {
            let name = if index == 0 {
                "root".to_owned()
            } else {
                let name = name.as_deref().map(one_line).unwrap_or_default();
                if name.is_empty() {
                    "unnamed".to_owned()
                } else {
                    name
                }
            };
            format!("{name} (thread {id})")
        })
        .collect::<Vec<_>>()
        .join(" > ");
    let role = if rows.len() == 1 { "root" } else { "subagent" };
    let mut content = format!("Thread context:\n- position: {position}\n- role: {role}");
    if role == "subagent" {
        content.push_str(&format!(
            "\n- recursive delegation: {}",
            if allow_delegation {
                "allowed"
            } else {
                "denied"
            }
        ));
    }
    let payload = serde_json::to_string(&MessageEvent { content })
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    transaction.execute(
        "INSERT INTO events (thread_id, sequence, kind, payload) VALUES (?1, 0, 'thread_context', ?2)",
        params![thread_id, payload],
    )?;
    Ok(())
}

fn one_line(value: &str) -> String {
    let mut output = String::new();
    let mut separated = false;
    for character in value.chars() {
        if character.is_whitespace() || character.is_control() {
            separated = !output.is_empty();
        } else {
            if separated {
                output.push(' ');
                separated = false;
            }
            output.push(character);
        }
    }
    output
}

fn event_columns(data: &ThreadEventData) -> tokio_rusqlite::Result<(String, Value)> {
    let Value::Object(mut event) = serde_json::to_value(data).map_err(to_sql_error)? else {
        unreachable!("thread event data serializes as an object");
    };
    let kind = event
        .remove("kind")
        .and_then(|kind| kind.as_str().map(str::to_owned))
        .expect("thread event data has a kind");
    let payload = event
        .remove("payload")
        .expect("thread event data has a payload");
    Ok((kind, payload))
}

fn event_data(kind: &str, payload: &str) -> rusqlite::Result<ThreadEventData> {
    let payload: Value = serde_json::from_str(payload).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
    })?;
    serde_json::from_value(serde_json::json!({ "kind": kind, "payload": payload }))
        .map_err(|error| rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error)))
}

fn create_checkpoint(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: i64,
    created_at_ms: i64,
    reason: &str,
) -> rusqlite::Result<i64> {
    let (display_name, provider, model, reasoning_effort): (
        Option<String>,
        String,
        String,
        String,
    ) = transaction.query_row(
        "
            SELECT display_name, provider, model, reasoning_effort
            FROM threads
            WHERE id = ?1
            ",
        [thread_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    transaction.execute(
        "
        INSERT INTO checkpoints (
            thread_id, created_at_ms, reason, display_name, provider, model, reasoning_effort
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            thread_id,
            created_at_ms,
            reason,
            display_name,
            provider,
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

enum HistoryPoint {
    UserMessage,
    AssistantMessage,
}

fn validate_history_point(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: i64,
    checkpoint_id: Option<i64>,
    sequence: i64,
) -> rusqlite::Result<HistoryPoint> {
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
    match kind.as_str() {
        "user_message" => Ok(HistoryPoint::UserMessage),
        "assistant_message" => Ok(HistoryPoint::AssistantMessage),
        _ => Err(rusqlite::Error::InvalidQuery),
    }
}

fn history_end_sequence(kind: HistoryPoint, sequence: i64) -> i64 {
    match kind {
        HistoryPoint::UserMessage => sequence - 1,
        HistoryPoint::AssistantMessage => sequence,
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

fn clone_compaction_checkpoints(
    transaction: &rusqlite::Transaction<'_>,
    source_thread_id: i64,
    destination_thread_id: i64,
) -> rusqlite::Result<()> {
    let compactions = {
        let mut statement = transaction.prepare(
            "SELECT sequence, payload FROM events WHERE thread_id = ?1 AND kind = 'compaction' ORDER BY sequence",
        )?;
        statement
            .query_map([destination_thread_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut cloned = HashMap::new();
    let mut visiting = HashSet::new();
    let mut previous_sequence = None;
    for (sequence, payload) in compactions {
        if previous_sequence.is_some_and(|previous| previous >= sequence) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        previous_sequence = Some(sequence);
        let mut compaction: CompactionEvent = serde_json::from_str(&payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(error))
        })?;
        let (checkpoint_id, checkpoint_max) = clone_checkpoint_chain(
            transaction,
            source_thread_id,
            destination_thread_id,
            compaction.checkpoint_id.0,
            &mut cloned,
            &mut visiting,
        )?;
        if checkpoint_max.is_some_and(|checkpoint_max| checkpoint_max >= sequence) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        compaction.checkpoint_id = CheckpointId(checkpoint_id);
        transaction.execute(
            "UPDATE events SET payload = ?1 WHERE thread_id = ?2 AND sequence = ?3",
            params![
                serde_json::to_string(&compaction)
                    .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?,
                destination_thread_id,
                sequence
            ],
        )?;
    }
    Ok(())
}

fn clone_checkpoint_chain(
    transaction: &rusqlite::Transaction<'_>,
    source_thread_id: i64,
    destination_thread_id: i64,
    source_checkpoint_id: i64,
    cloned: &mut HashMap<i64, (i64, Option<i64>)>,
    visiting: &mut HashSet<i64>,
) -> rusqlite::Result<(i64, Option<i64>)> {
    if let Some(cloned) = cloned.get(&source_checkpoint_id) {
        return Ok(*cloned);
    }
    if !visiting.insert(source_checkpoint_id) {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let metadata: (i64, i64, String, Option<String>, String, String, String) = transaction.query_row(
        "SELECT thread_id, created_at_ms, reason, display_name, provider, model, reasoning_effort FROM checkpoints WHERE id = ?1",
        [source_checkpoint_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
    )?;
    if metadata.0 != source_thread_id {
        return Err(rusqlite::Error::InvalidQuery);
    }
    let mut events = {
        let mut statement = transaction.prepare(
            "SELECT sequence, kind, payload FROM checkpoint_events WHERE checkpoint_id = ?1 ORDER BY sequence",
        )?;
        statement
            .query_map([source_checkpoint_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut previous_sequence = None;
    for (sequence, kind, payload) in &mut events {
        if previous_sequence.is_some_and(|previous| previous >= *sequence) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        previous_sequence = Some(*sequence);
        if kind != "compaction" {
            continue;
        }
        let mut compaction: CompactionEvent = serde_json::from_str(payload).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(error))
        })?;
        let (checkpoint_id, checkpoint_max) = clone_checkpoint_chain(
            transaction,
            source_thread_id,
            destination_thread_id,
            compaction.checkpoint_id.0,
            cloned,
            visiting,
        )?;
        if checkpoint_max.is_some_and(|checkpoint_max| checkpoint_max >= *sequence) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        compaction.checkpoint_id = CheckpointId(checkpoint_id);
        *payload = serde_json::to_string(&compaction)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    }
    transaction.execute(
        "INSERT INTO checkpoints (thread_id, created_at_ms, reason, display_name, provider, model, reasoning_effort) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![destination_thread_id, metadata.1, metadata.2, metadata.3, metadata.4, metadata.5, metadata.6],
    )?;
    let new_id = transaction.last_insert_rowid();
    for (sequence, kind, payload) in events {
        transaction.execute(
            "INSERT INTO checkpoint_events (checkpoint_id, sequence, kind, payload) VALUES (?1, ?2, ?3, ?4)",
            params![new_id, sequence, kind, payload],
        )?;
    }
    visiting.remove(&source_checkpoint_id);
    let cloned_checkpoint = (new_id, previous_sequence);
    cloned.insert(source_checkpoint_id, cloned_checkpoint);
    Ok(cloned_checkpoint)
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
            sequence: EventSequence(sequence),
            data: event_data(&kind, &payload)?,
        })
    })
    .collect()
}

fn read_event_rows<P: rusqlite::Params>(
    connection: &rusqlite::Connection,
    sql: &str,
    parameters: P,
) -> rusqlite::Result<Vec<Event>> {
    let mut statement = connection.prepare(sql)?;
    statement
        .query_map(parameters, |row| {
            let sequence = EventSequence(row.get(0)?);
            let kind: String = row.get(1)?;
            let payload: String = row.get(2)?;
            Ok(Event {
                sequence,
                data: event_data(&kind, &payload)?,
            })
        })?
        .collect()
}

fn reconstruct_events(
    transaction: &rusqlite::Transaction<'_>,
    thread_id: i64,
    events: Vec<Event>,
    output: &mut Vec<Event>,
    visiting: &mut HashSet<i64>,
) -> rusqlite::Result<()> {
    let mut previous_sequence = None;
    for event in events {
        if previous_sequence.is_some_and(|previous| previous >= event.sequence) {
            return Err(rusqlite::Error::InvalidQuery);
        }
        previous_sequence = Some(event.sequence);
        if matches!(event.data, ThreadEventData::ThreadContext(_))
            && output
                .iter()
                .any(|event| matches!(event.data, ThreadEventData::ThreadContext(_)))
        {
            continue;
        }
        if let ThreadEventData::Compaction(compaction) = &event.data {
            if !visiting.insert(compaction.checkpoint_id.0) {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let owner: i64 = transaction.query_row(
                "SELECT thread_id FROM checkpoints WHERE id = ?1",
                [compaction.checkpoint_id.0],
                |row| row.get(0),
            )?;
            if owner != thread_id {
                return Err(rusqlite::Error::InvalidQuery);
            }
            let checkpoint_events = read_event_rows(
                transaction,
                "SELECT sequence, kind, payload FROM checkpoint_events WHERE checkpoint_id = ?1 ORDER BY sequence",
                [compaction.checkpoint_id.0],
            )?;
            reconstruct_events(transaction, thread_id, checkpoint_events, output, visiting)?;
            visiting.remove(&compaction.checkpoint_id.0);
        }
        if output
            .last()
            .is_some_and(|previous| previous.sequence >= event.sequence)
        {
            return Err(rusqlite::Error::InvalidQuery);
        }
        output.push(event);
    }
    Ok(())
}

#[cfg(test)]
impl Store {
    async fn delete_thread(&self, thread_id: ThreadId) -> tokio_rusqlite::Result<()> {
        self.delete_threads(vec![thread_id]).await
    }

    async fn report_events(
        &self,
        thread_id: ThreadId,
        through: EventSequence,
    ) -> tokio_rusqlite::Result<Vec<Event>> {
        Ok(self
            .report_snapshot(thread_id)
            .await?
            .events
            .into_iter()
            .filter(|event| event.sequence <= through)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: &str) -> ThreadEventData {
        ThreadEventData::UserMessage(atra_protocol::MessageEvent {
            content: content.to_owned(),
        })
    }

    fn assistant(content: &str) -> ThreadEventData {
        ThreadEventData::AssistantMessage(atra_protocol::AssistantMessageEvent {
            content: content.to_owned(),
            phase: Some(atra_protocol::AssistantMessagePhase::FinalAnswer),
            todos: Vec::new(),
        })
    }

    #[tokio::test]
    async fn child_hierarchy_and_forks_preserve_ownership() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let root = store
            .create_thread(
                Some("root".into()),
                "fake".into(),
                "model".into(),
                "medium".into(),
            )
            .await
            .unwrap();
        let child = store
            .create_child_thread(
                root,
                "child\nforged".into(),
                "fake".into(),
                "model".into(),
                "medium".into(),
                true,
            )
            .await
            .unwrap();
        let grandchild = store
            .create_child_thread(
                child,
                "grandchild".into(),
                "fake".into(),
                "model".into(),
                "medium".into(),
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            store.descendants(root).await.unwrap(),
            vec![child, grandchild]
        );
        assert_eq!(store.root_thread(grandchild).await.unwrap(), root);
        let root_context = &store.events(root).await.unwrap()[0].data;
        assert_eq!(
            root_context,
            &ThreadEventData::ThreadContext(MessageEvent {
                content: format!(
                    "Thread context:\n- position: root (thread {})\n- role: root",
                    root.0
                ),
            })
        );
        let child_context = &store.events(child).await.unwrap()[0].data;
        assert_eq!(
            child_context,
            &ThreadEventData::ThreadContext(MessageEvent {
                content: format!(
                    "Thread context:\n- position: root (thread {}) > child forged (thread {})\n- role: subagent\n- recursive delegation: allowed",
                    root.0, child.0
                ),
            })
        );
        store
            .append_all(child, vec![user("question"), assistant("answer")])
            .await
            .unwrap();
        let fork = store
            .fork_thread(child, None, EventSequence(2), Some("fork".into()))
            .await
            .unwrap();
        assert_eq!(
            store.thread(fork).await.unwrap().parent_thread_id,
            Some(root)
        );
        assert!(store.delegation_allowed(fork).await.unwrap());
        assert_eq!(
            store.events(fork).await.unwrap()[0].data,
            store.events(child).await.unwrap()[0].data
        );
    }

    #[tokio::test]
    async fn compaction_cursors_are_monotonic_and_forks_own_checkpoint_chains() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let source = store
            .create_thread(None, "fake".into(), "model".into(), "medium".into())
            .await
            .unwrap();
        store
            .append_all(source, vec![user("before"), assistant("kept")])
            .await
            .unwrap();
        let checkpoint = store
            .create_checkpoint(source, 1, "compaction".into())
            .await
            .unwrap();
        store
            .replace_with_compaction(
                source,
                CompactionEvent {
                    items: serde_json::json!([]),
                    checkpoint_id: checkpoint,
                },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .events(source)
                .await
                .unwrap()
                .iter()
                .map(|event| event.data.kind())
                .collect::<Vec<_>>(),
            ["thread_context", "compaction"]
        );
        let sequences = store
            .append_all(source, vec![user("after"), assistant("done")])
            .await
            .unwrap();
        assert_eq!(sequences, vec![EventSequence(4), EventSequence(5)]);
        let report = store.report_events(source, EventSequence(5)).await.unwrap();
        assert_eq!(
            report
                .iter()
                .map(|event| event.sequence.0)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 4, 5]
        );

        let second_checkpoint = store
            .create_checkpoint(source, 2, "compaction".into())
            .await
            .unwrap();
        store
            .replace_with_compaction(
                source,
                CompactionEvent {
                    items: serde_json::json!([]),
                    checkpoint_id: second_checkpoint,
                },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .events(source)
                .await
                .unwrap()
                .iter()
                .map(|event| event.data.kind())
                .collect::<Vec<_>>(),
            ["thread_context", "compaction"]
        );
        store
            .append_all(source, vec![user("again"), assistant("finished")])
            .await
            .unwrap();

        let fork = store
            .fork_thread(source, None, EventSequence(8), Some("fork".into()))
            .await
            .unwrap();
        let events = store.events(fork).await.unwrap();
        let Some(ThreadEventData::Compaction(compaction)) = events
            .iter()
            .find(|event| matches!(event.data, ThreadEventData::Compaction(_)))
            .map(|event| &event.data)
        else {
            panic!("missing compaction")
        };
        assert_ne!(compaction.checkpoint_id, second_checkpoint);
        assert_eq!(
            store
                .checkpoint(compaction.checkpoint_id)
                .await
                .unwrap()
                .thread_id,
            fork
        );
        store.delete_thread(source).await.unwrap();
        assert_eq!(
            store
                .report_events(fork, EventSequence(8))
                .await
                .unwrap()
                .len(),
            9
        );
    }

    #[tokio::test]
    async fn report_and_fork_reject_a_cyclic_checkpoint_chain() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let thread = store
            .create_thread(None, "fake".into(), "model".into(), "medium".into())
            .await
            .unwrap();
        store.append(thread, user("first")).await.unwrap();
        let first = store
            .create_checkpoint(thread, 1, "compaction".into())
            .await
            .unwrap();
        store
            .replace_with_compaction(
                thread,
                CompactionEvent {
                    items: serde_json::json!([]),
                    checkpoint_id: first,
                },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let second = store
            .create_checkpoint(thread, 2, "compaction".into())
            .await
            .unwrap();
        store
            .replace_with_compaction(
                thread,
                CompactionEvent {
                    items: serde_json::json!([]),
                    checkpoint_id: second,
                },
                None,
                None,
                None,
            )
            .await
            .unwrap();
        let cycle = serde_json::to_string(&CompactionEvent {
            items: serde_json::json!([]),
            checkpoint_id: second,
        })
        .unwrap();
        store
            .connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE checkpoint_events SET kind = 'compaction', payload = ?1 WHERE checkpoint_id = ?2 AND sequence = 0",
                    params![cycle, first.0],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await
            .unwrap();

        assert!(store.report_snapshot(thread).await.is_err());
        let sequence = store.append(thread, user("fork point")).await.unwrap();
        assert!(
            store
                .fork_thread(thread, None, sequence, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn report_rejects_checkpoint_owned_by_another_thread() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let thread = store
            .create_thread(None, "fake".into(), "model".into(), "medium".into())
            .await
            .unwrap();
        let other = store
            .create_thread(None, "fake".into(), "model".into(), "medium".into())
            .await
            .unwrap();
        store.append(other, user("other")).await.unwrap();
        let checkpoint = store
            .create_checkpoint(other, 1, "compaction".into())
            .await
            .unwrap();
        store
            .append(
                thread,
                ThreadEventData::Compaction(CompactionEvent {
                    items: serde_json::json!([]),
                    checkpoint_id: checkpoint,
                }),
            )
            .await
            .unwrap();

        assert!(store.report_snapshot(thread).await.is_err());
    }

    #[tokio::test]
    async fn report_rejects_missing_and_corrupt_checkpoint_links() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let thread = store
            .create_thread(None, "fake".into(), "model".into(), "medium".into())
            .await
            .unwrap();
        store
            .append(
                thread,
                ThreadEventData::Compaction(CompactionEvent {
                    items: serde_json::json!([]),
                    checkpoint_id: CheckpointId(999),
                }),
            )
            .await
            .unwrap();
        assert!(store.report_snapshot(thread).await.is_err());

        store
            .connection
            .call(move |connection| {
                connection.execute(
                    "UPDATE events SET payload = 'not-json' WHERE thread_id = ?1 AND kind = 'compaction'",
                    [thread.0],
                )?;
                Ok::<_, rusqlite::Error>(())
            })
            .await
            .unwrap();
        assert!(store.report_snapshot(thread).await.is_err());
    }

    #[tokio::test]
    async fn events_keep_their_thread_order() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let thread = store
            .create_thread(
                None,
                "fake".to_owned(),
                "test-model".to_owned(),
                "medium".to_owned(),
            )
            .await
            .unwrap();

        assert_eq!(
            store
                .append(
                    thread,
                    ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                        content: "one".to_owned()
                    })
                )
                .await
                .unwrap(),
            EventSequence(1)
        );
        assert_eq!(
            store
                .append(
                    thread,
                    ThreadEventData::AssistantMessage(atra_protocol::AssistantMessageEvent {
                        content: "two".to_owned(),
                        phase: None,
                        todos: Vec::new(),
                    })
                )
                .await
                .unwrap(),
            EventSequence(2)
        );
        assert_eq!(
            store.events(thread).await.unwrap(),
            vec![
                Event {
                    sequence: EventSequence(0),
                    data: ThreadEventData::ThreadContext(atra_protocol::MessageEvent {
                        content: format!(
                            "Thread context:\n- position: root (thread {})\n- role: root",
                            thread.0
                        )
                    }),
                },
                Event {
                    sequence: EventSequence(1),
                    data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                        content: "one".to_owned()
                    }),
                },
                Event {
                    sequence: EventSequence(2),
                    data: ThreadEventData::AssistantMessage(atra_protocol::AssistantMessageEvent {
                        content: "two".to_owned(),
                        phase: None,
                        todos: Vec::new(),
                    }),
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
            .create_thread(
                None,
                "fake".to_owned(),
                "test-model".to_owned(),
                "medium".to_owned(),
            )
            .await
            .unwrap();
        store
            .append(
                thread,
                ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                    content: "saved".to_owned(),
                }),
            )
            .await
            .unwrap();
        drop(store);

        let reopened = Store::open(&database).await.unwrap();

        assert_eq!(
            reopened.events(thread).await.unwrap(),
            vec![
                Event {
                    sequence: EventSequence(0),
                    data: ThreadEventData::ThreadContext(atra_protocol::MessageEvent {
                        content: format!(
                            "Thread context:\n- position: root (thread {})\n- role: root",
                            thread.0
                        )
                    }),
                },
                Event {
                    sequence: EventSequence(1),
                    data: ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                        content: "saved".to_owned()
                    }),
                }
            ]
        );
    }

    #[tokio::test]
    async fn threads_are_listed_newest_first() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let first = store
            .create_thread(
                Some("First".to_owned()),
                "fake".to_owned(),
                "model-a".to_owned(),
                "low".to_owned(),
            )
            .await
            .unwrap();
        let second = store
            .create_thread(
                None,
                "fake".to_owned(),
                "model-b".to_owned(),
                "high".to_owned(),
            )
            .await
            .unwrap();

        assert_eq!(
            store.threads().await.unwrap(),
            vec![
                Thread {
                    id: second,
                    parent_thread_id: None,
                    display_name: None,
                    provider: "fake".to_owned(),
                    model: "model-b".to_owned(),
                    reasoning_effort: "high".to_owned(),
                },
                Thread {
                    id: first,
                    parent_thread_id: None,
                    display_name: Some("First".to_owned()),
                    provider: "fake".to_owned(),
                    model: "model-a".to_owned(),
                    reasoning_effort: "low".to_owned(),
                },
            ]
        );
    }

    #[tokio::test]
    async fn delete_thread_removes_events_and_checkpoints() {
        let store = Store::open(Path::new(":memory:")).await.unwrap();
        let thread = store
            .create_thread(
                Some("To delete".to_owned()),
                "fake".to_owned(),
                "model-a".to_owned(),
                "low".to_owned(),
            )
            .await
            .unwrap();
        store
            .append(
                thread,
                ThreadEventData::UserMessage(atra_protocol::MessageEvent {
                    content: "hello".to_owned(),
                }),
            )
            .await
            .unwrap();
        store
            .create_checkpoint(thread, 1_000, "manual".to_owned())
            .await
            .unwrap();

        store.delete_thread(thread).await.unwrap();

        assert!(store.threads().await.unwrap().is_empty());
        assert!(store.events(thread).await.unwrap().is_empty());
        assert!(store.checkpoints(thread).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn deleted_thread_ids_are_never_reused() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("state.db"))
            .await
            .unwrap();
        let first = store
            .create_thread(None, "fake".into(), "test".into(), "medium".into())
            .await
            .unwrap();
        store.delete_thread(first).await.unwrap();
        let second = store
            .create_thread(None, "fake".into(), "test".into(), "medium".into())
            .await
            .unwrap();

        assert!(second.0 > first.0);
    }
}
