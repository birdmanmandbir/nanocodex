use std::{path::Path, time::Instant};

use chrono::{DateTime, Utc};
use nanocodex_agent::session::SessionSnapshot;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};

use crate::{
    AgentEvent, AgentEventPayload, ContentBlock, ForkSource, TurnAction, TurnActionResponse,
    TurnDelivery, TurnStatus, TurnView,
};

const COMMAND_CAPACITY: usize = 256;
const FULL_CHECKPOINT_INTERVAL: i64 = 32;

#[derive(Clone)]
pub(crate) struct SessionStore {
    sender: mpsc::Sender<CommandEnvelope>,
}

struct CommandEnvelope {
    dispatch: tracing::Dispatch,
    parent: tracing::Span,
    queued_at: Instant,
    command: Command,
}

pub(crate) struct StoredSession {
    pub turns: Vec<StoredTurn>,
    pub snapshot: Option<SessionSnapshot>,
}

pub(crate) struct StoredTurn {
    pub view: TurnView,
    pub inputs: Vec<Vec<ContentBlock>>,
    pub cancel_requested: bool,
}

pub(crate) struct NewTurn {
    pub view: TurnView,
    pub delivery: TurnDelivery,
    pub content: Vec<ContentBlock>,
    pub response: TurnActionResponse,
    pub idempotency_key: Option<String>,
    pub request_hash: Vec<u8>,
    pub payment_receipt: Option<String>,
    pub event: AgentEventPayload,
}

pub(crate) struct StoredRequest {
    pub response: TurnActionResponse,
    pub request_hash: Option<Vec<u8>>,
    pub payment_receipt: Option<String>,
}

pub(crate) struct SteerRequestRecord {
    pub content: Vec<ContentBlock>,
    pub response: TurnActionResponse,
    pub idempotency_key: Option<String>,
    pub request_hash: Vec<u8>,
    pub payment_receipt: Option<String>,
}

#[derive(Clone)]
pub(crate) struct CompletedTurn {
    pub status: TurnStatus,
    pub output: Vec<ContentBlock>,
    pub error: Option<String>,
    pub completed_at: DateTime<Utc>,
    pub snapshot: Option<SessionSnapshot>,
    pub usage: Option<nanocodex_agent::TurnUsage>,
    pub event: AgentEventPayload,
}

pub(crate) struct FinishedTurn {
    pub event: AgentEvent,
    pub snapshot: Option<SessionSnapshot>,
}

impl SessionStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        let connection = Connection::open(path)?;
        configure(&connection)?;
        let (sender, mut receiver) = mpsc::channel::<CommandEnvelope>(COMMAND_CAPACITY);
        tokio::task::spawn_blocking(move || {
            while let Some(envelope) = receiver.blocking_recv() {
                let command = envelope.command.name();
                let span = tracing::dispatcher::with_default(&envelope.dispatch, || {
                    tracing::info_span!(
                        parent: &envelope.parent,
                        "nanocentaur.sqlite.command",
                        db.system = "sqlite",
                        db.operation.name = command,
                        queue.duration_ns = u64::try_from(
                            envelope.queued_at.elapsed().as_nanos()
                        )
                        .unwrap_or(u64::MAX),
                    )
                });
                let _entered = span.enter();
                envelope.command.execute(&connection);
            }
        });
        Ok(Self { sender })
    }

    pub async fn load(&self, agent_id: String) -> Result<StoredSession, SessionError> {
        self.call(|reply| Command::Load { agent_id, reply }).await
    }

    pub async fn find_request(
        &self,
        agent_id: String,
        key: String,
    ) -> Result<Option<StoredRequest>, SessionError> {
        self.call(|reply| Command::FindRequest {
            agent_id,
            key,
            reply,
        })
        .await
    }

    pub async fn turn(
        &self,
        agent_id: String,
        turn_id: String,
    ) -> Result<Option<TurnView>, SessionError> {
        self.call(|reply| Command::GetTurn {
            agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn record_turn(
        &self,
        agent_id: String,
        turn: NewTurn,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::RecordTurn {
            agent_id,
            turn: Box::new(turn),
            reply,
        })
        .await
    }

    pub async fn record_steer(
        &self,
        agent_id: String,
        turn_id: String,
        record: SteerRequestRecord,
    ) -> Result<i64, SessionError> {
        self.call(|reply| Command::RecordSteer {
            agent_id,
            turn_id,
            record: Box::new(record),
            reply,
        })
        .await
    }

    pub async fn undo_steer(
        &self,
        agent_id: String,
        turn_id: String,
        ordinal: i64,
        idempotency_key: Option<String>,
    ) -> Result<(), SessionError> {
        self.call(|reply| Command::UndoSteer {
            agent_id,
            turn_id,
            ordinal,
            idempotency_key,
            reply,
        })
        .await
    }

    pub async fn mark_started(
        &self,
        agent_id: String,
        turn_id: String,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::MarkStarted {
            agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn request_cancel(
        &self,
        agent_id: String,
        turn_id: String,
    ) -> Result<AgentEvent, SessionError> {
        self.call(|reply| Command::RequestCancel {
            agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn finish_turn(
        &self,
        agent_id: String,
        turn_id: String,
        completed: CompletedTurn,
    ) -> Result<FinishedTurn, SessionError> {
        self.call(|reply| Command::FinishTurn {
            agent_id,
            turn_id,
            completed: Box::new(completed),
            reply,
        })
        .await
    }

    pub async fn append_events(
        &self,
        agent_id: String,
        events: Vec<(Option<String>, AgentEventPayload)>,
    ) -> Result<Vec<AgentEvent>, SessionError> {
        self.call(|reply| Command::AppendEvents {
            agent_id,
            events,
            reply,
        })
        .await
    }

    pub async fn events_after(
        &self,
        agent_id: String,
        after_event_id: u64,
        limit: usize,
    ) -> Result<Vec<AgentEvent>, SessionError> {
        self.call(|reply| Command::EventsAfter {
            agent_id,
            after_event_id,
            limit,
            reply,
        })
        .await
    }

    pub async fn fork(
        &self,
        source_agent_id: String,
        target_agent_id: String,
        turn_id: Option<String>,
    ) -> Result<ForkSource, SessionError> {
        self.call(|reply| Command::Fork {
            source_agent_id,
            target_agent_id,
            turn_id,
            reply,
        })
        .await
    }

    pub async fn delete(&self, agent_id: String) -> Result<(), SessionError> {
        self.call(|reply| Command::Delete { agent_id, reply }).await
    }

    async fn call<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, SessionError>>) -> Command,
    ) -> Result<T, SessionError> {
        let (reply, response) = oneshot::channel();
        let command = command(reply);
        self.sender
            .send(CommandEnvelope {
                dispatch: tracing::dispatcher::get_default(Clone::clone),
                parent: tracing::Span::current(),
                queued_at: Instant::now(),
                command,
            })
            .await
            .map_err(|_| SessionError::Stopped)?;
        response.await.map_err(|_| SessionError::Stopped)?
    }
}

enum Command {
    Load {
        agent_id: String,
        reply: Reply<StoredSession>,
    },
    FindRequest {
        agent_id: String,
        key: String,
        reply: Reply<Option<StoredRequest>>,
    },
    GetTurn {
        agent_id: String,
        turn_id: String,
        reply: Reply<Option<TurnView>>,
    },
    RecordTurn {
        agent_id: String,
        turn: Box<NewTurn>,
        reply: Reply<AgentEvent>,
    },
    RecordSteer {
        agent_id: String,
        turn_id: String,
        record: Box<SteerRequestRecord>,
        reply: Reply<i64>,
    },
    UndoSteer {
        agent_id: String,
        turn_id: String,
        ordinal: i64,
        idempotency_key: Option<String>,
        reply: Reply<()>,
    },
    MarkStarted {
        agent_id: String,
        turn_id: String,
        reply: Reply<AgentEvent>,
    },
    RequestCancel {
        agent_id: String,
        turn_id: String,
        reply: Reply<AgentEvent>,
    },
    FinishTurn {
        agent_id: String,
        turn_id: String,
        completed: Box<CompletedTurn>,
        reply: Reply<FinishedTurn>,
    },
    AppendEvents {
        agent_id: String,
        events: Vec<(Option<String>, AgentEventPayload)>,
        reply: Reply<Vec<AgentEvent>>,
    },
    EventsAfter {
        agent_id: String,
        after_event_id: u64,
        limit: usize,
        reply: Reply<Vec<AgentEvent>>,
    },
    Fork {
        source_agent_id: String,
        target_agent_id: String,
        turn_id: Option<String>,
        reply: Reply<ForkSource>,
    },
    Delete {
        agent_id: String,
        reply: Reply<()>,
    },
}

type Reply<T> = oneshot::Sender<Result<T, SessionError>>;

impl Command {
    const fn name(&self) -> &'static str {
        match self {
            Self::Load { .. } => "load",
            Self::FindRequest { .. } => "find_request",
            Self::GetTurn { .. } => "get_turn",
            Self::RecordTurn { .. } => "record_turn",
            Self::RecordSteer { .. } => "record_steer",
            Self::UndoSteer { .. } => "undo_steer",
            Self::MarkStarted { .. } => "mark_started",
            Self::RequestCancel { .. } => "request_cancel",
            Self::FinishTurn { .. } => "finish_turn",
            Self::AppendEvents { .. } => "append_events",
            Self::EventsAfter { .. } => "events_after",
            Self::Fork { .. } => "fork",
            Self::Delete { .. } => "delete",
        }
    }

    fn execute(self, connection: &Connection) {
        match self {
            Self::Load { agent_id, reply } => {
                drop(reply.send(load(connection, &agent_id)));
            }
            Self::FindRequest {
                agent_id,
                key,
                reply,
            } => {
                drop(reply.send(find_request(connection, &agent_id, &key)));
            }
            Self::GetTurn {
                agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(turn(connection, &agent_id, &turn_id)));
            }
            Self::RecordTurn {
                agent_id,
                turn,
                reply,
            } => {
                drop(reply.send(record_turn(connection, &agent_id, *turn)));
            }
            Self::RecordSteer {
                agent_id,
                turn_id,
                record,
                reply,
            } => {
                drop(reply.send(record_steer(connection, &agent_id, &turn_id, &record)));
            }
            Self::UndoSteer {
                agent_id,
                turn_id,
                ordinal,
                idempotency_key,
                reply,
            } => {
                drop(reply.send(undo_steer(
                    connection,
                    &agent_id,
                    &turn_id,
                    ordinal,
                    idempotency_key.as_deref(),
                )));
            }
            Self::MarkStarted {
                agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(mark_started(connection, &agent_id, &turn_id)));
            }
            Self::RequestCancel {
                agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(request_cancel(connection, &agent_id, &turn_id)));
            }
            Self::FinishTurn {
                agent_id,
                turn_id,
                completed,
                reply,
            } => {
                drop(reply.send(finish_turn(connection, &agent_id, &turn_id, *completed)));
            }
            Self::AppendEvents {
                agent_id,
                events,
                reply,
            } => {
                drop(reply.send(append_events(connection, &agent_id, events)));
            }
            Self::EventsAfter {
                agent_id,
                after_event_id,
                limit,
                reply,
            } => {
                drop(reply.send(events_after(connection, &agent_id, after_event_id, limit)));
            }
            Self::Fork {
                source_agent_id,
                target_agent_id,
                turn_id,
                reply,
            } => {
                drop(reply.send(fork(
                    connection,
                    &source_agent_id,
                    &target_agent_id,
                    turn_id.as_deref(),
                )));
            }
            Self::Delete { agent_id, reply } => {
                drop(reply.send(delete(connection, &agent_id)));
            }
        }
    }
}

fn configure(connection: &Connection) -> Result<(), SessionError> {
    connection.pragma_update(None, "foreign_keys", "ON")?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch(
        r"
        CREATE TABLE IF NOT EXISTS session_agents (
            id TEXT PRIMARY KEY,
            created_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS session_tombstones (
            agent_id TEXT PRIMARY KEY,
            deleted_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS turns (
            agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
            id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            delivery TEXT NOT NULL,
            state TEXT NOT NULL,
            output_json TEXT NOT NULL DEFAULT '[]',
            usage_json TEXT,
            error TEXT,
            cancel_requested INTEGER NOT NULL DEFAULT 0,
            attempt INTEGER NOT NULL DEFAULT 0,
            checkpoint_json TEXT,
            checkpoint_base_ordinal INTEGER,
            checkpoint_prefix INTEGER,
            checkpoint_suffix INTEGER,
            checkpoint_data BLOB,
            completion_event_id INTEGER,
            created_at TEXT NOT NULL,
            completed_at TEXT,
            PRIMARY KEY(agent_id, id),
            UNIQUE(agent_id, ordinal)
        );

        CREATE TABLE IF NOT EXISTS turn_inputs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            agent_id TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            ordinal INTEGER NOT NULL,
            content_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            FOREIGN KEY(agent_id, turn_id) REFERENCES turns(agent_id, id) ON DELETE CASCADE,
            UNIQUE(agent_id, turn_id, ordinal)
        );

        CREATE TABLE IF NOT EXISTS turn_requests (
            agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
            idempotency_key TEXT NOT NULL,
            action TEXT NOT NULL,
            turn_id TEXT NOT NULL,
            state TEXT NOT NULL,
            request_hash BLOB,
            payment_receipt TEXT,
            created_at TEXT NOT NULL,
            PRIMARY KEY(agent_id, idempotency_key)
        );

        CREATE TABLE IF NOT EXISTS session_events (
            agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
            id INTEGER NOT NULL,
            turn_id TEXT,
            payload_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            PRIMARY KEY(agent_id, id)
        );

        CREATE TABLE IF NOT EXISTS session_forks (
            agent_id TEXT PRIMARY KEY REFERENCES session_agents(id) ON DELETE CASCADE,
            source_agent_id TEXT NOT NULL,
            source_turn_id TEXT NOT NULL,
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS turns_agent_state
            ON turns(agent_id, state, ordinal);
        CREATE INDEX IF NOT EXISTS events_agent_id
            ON session_events(agent_id, id);
        ",
    )?;
    ensure_turn_usage_column(connection)?;
    ensure_turn_request_columns(connection)?;
    ensure_checkpoint_columns(connection)?;
    Ok(())
}

fn ensure_turn_usage_column(connection: &Connection) -> Result<(), SessionError> {
    let mut statement = connection.prepare("PRAGMA table_info(turns)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "usage_json") {
        connection.execute_batch("ALTER TABLE turns ADD COLUMN usage_json TEXT")?;
    }
    Ok(())
}

fn ensure_turn_request_columns(connection: &Connection) -> Result<(), SessionError> {
    let mut statement = connection.prepare("PRAGMA table_info(turn_requests)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == "request_hash") {
        connection.execute_batch("ALTER TABLE turn_requests ADD COLUMN request_hash BLOB")?;
    }
    if !columns.iter().any(|column| column == "payment_receipt") {
        connection.execute_batch("ALTER TABLE turn_requests ADD COLUMN payment_receipt TEXT")?;
    }
    Ok(())
}

fn ensure_checkpoint_columns(connection: &Connection) -> Result<(), SessionError> {
    let mut statement = connection.prepare("PRAGMA table_info(turns)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    for (name, definition) in [
        ("checkpoint_base_ordinal", "INTEGER"),
        ("checkpoint_prefix", "INTEGER"),
        ("checkpoint_suffix", "INTEGER"),
        ("checkpoint_data", "BLOB"),
    ] {
        if !columns.iter().any(|column| column == name) {
            connection
                .execute_batch(&format!("ALTER TABLE turns ADD COLUMN {name} {definition}"))?;
        }
    }
    Ok(())
}

fn ensure_agent(connection: &Connection, agent_id: &str) -> Result<(), SessionError> {
    reject_tombstone(connection, agent_id)?;
    connection.execute(
        "INSERT OR IGNORE INTO session_agents (id, created_at) VALUES (?1, ?2)",
        params![agent_id, now()],
    )?;
    Ok(())
}

fn load(connection: &Connection, agent_id: &str) -> Result<StoredSession, SessionError> {
    ensure_agent(connection, agent_id)?;
    let transaction = connection.unchecked_transaction()?;
    let interrupted = {
        let mut statement = transaction.prepare(
            "SELECT id FROM turns
             WHERE agent_id = ?1 AND state = 'running'
             ORDER BY ordinal",
        )?;
        statement
            .query_map(params![agent_id], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for turn_id in interrupted {
        transaction.execute(
            "UPDATE turns SET state = 'queued' WHERE agent_id = ?1 AND id = ?2",
            params![agent_id, turn_id],
        )?;
        append_event_tx(
            &transaction,
            agent_id,
            Some(&turn_id),
            AgentEventPayload::TurnInterrupted { retrying: true },
        )?;
    }
    transaction.commit()?;

    let mut statement = connection.prepare(
        "SELECT id, state, cancel_requested, created_at
         FROM turns
         WHERE agent_id = ?1 AND state IN ('queued', 'running')
         ORDER BY ordinal",
    )?;
    let turns = statement
        .query_map(params![agent_id], |row| {
            let turn_id: String = row.get(0)?;
            let state: String = row.get(1)?;
            let created_at: String = row.get(3)?;
            Ok(StoredTurn {
                view: TurnView {
                    turn_id: turn_id.clone(),
                    agent_id: agent_id.to_owned(),
                    state: parse_status(&state)?,
                    output: Vec::new(),
                    error: None,
                    usage: None,
                    created_at: parse_time(&created_at)?,
                    completed_at: None,
                },
                inputs: load_inputs(connection, agent_id, &turn_id)?,
                cancel_requested: row.get(2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let snapshot = reconstruct_checkpoint(connection, agent_id, None)?
        .as_deref()
        .map(serde_json::from_slice)
        .transpose()?;
    Ok(StoredSession { turns, snapshot })
}

fn load_inputs(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> rusqlite::Result<Vec<Vec<ContentBlock>>> {
    let mut statement = connection.prepare(
        "SELECT content_json FROM turn_inputs
         WHERE agent_id = ?1 AND turn_id = ?2 ORDER BY ordinal",
    )?;
    statement
        .query_map(params![agent_id, turn_id], |row| {
            let encoded: String = row.get(0)?;
            json_from_sql(&encoded)
        })?
        .collect()
}

fn find_request(
    connection: &Connection,
    agent_id: &str,
    key: &str,
) -> Result<Option<StoredRequest>, SessionError> {
    connection
        .query_row(
            "SELECT action, turn_id, state, request_hash, payment_receipt FROM turn_requests
             WHERE agent_id = ?1 AND idempotency_key = ?2",
            params![agent_id, key],
            |row| {
                let action: String = row.get(0)?;
                let state: String = row.get(2)?;
                Ok(StoredRequest {
                    response: TurnActionResponse {
                        action: parse_action(&action)?,
                        turn_id: row.get(1)?,
                        state: parse_status(&state)?,
                    },
                    request_hash: row.get(3)?,
                    payment_receipt: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn turn(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> Result<Option<TurnView>, SessionError> {
    connection
        .query_row(
            "SELECT state, output_json, usage_json, error, created_at, completed_at
             FROM turns WHERE agent_id = ?1 AND id = ?2",
            params![agent_id, turn_id],
            |row| {
                let state: String = row.get(0)?;
                let output_json: String = row.get(1)?;
                let usage_json: Option<String> = row.get(2)?;
                let created_at: String = row.get(4)?;
                let completed_at: Option<String> = row.get(5)?;
                Ok(TurnView {
                    turn_id: turn_id.to_owned(),
                    agent_id: agent_id.to_owned(),
                    state: parse_status(&state)?,
                    output: json_from_sql(&output_json)?,
                    error: row.get(3)?,
                    usage: usage_json.as_deref().map(json_from_sql).transpose()?,
                    created_at: parse_time(&created_at)?,
                    completed_at: completed_at.as_deref().map(parse_time).transpose()?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
}

fn record_turn(
    connection: &Connection,
    agent_id: &str,
    turn: NewTurn,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    ensure_agent_tx(&transaction, agent_id)?;
    let ordinal: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM turns WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;
    transaction.execute(
        "INSERT INTO turns
         (agent_id, id, ordinal, delivery, state, output_json, error,
          cancel_requested, attempt, created_at, completed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10)",
        params![
            agent_id,
            turn.view.turn_id,
            ordinal,
            delivery_name(turn.delivery),
            status_name(turn.view.state),
            serde_json::to_string(&turn.view.output)?,
            turn.view.error,
            i64::from(turn.view.state == TurnStatus::Running),
            turn.view.created_at.to_rfc3339(),
            turn.view.completed_at.map(|time| time.to_rfc3339()),
        ],
    )?;
    insert_input(&transaction, agent_id, &turn.view.turn_id, 1, &turn.content)?;
    if let Some(key) = turn.idempotency_key {
        insert_request(
            &transaction,
            agent_id,
            &key,
            &turn.response,
            &turn.request_hash,
            turn.payment_receipt.as_deref(),
        )?;
    }
    let event = append_event_tx(&transaction, agent_id, Some(&turn.view.turn_id), turn.event)?;
    transaction.commit()?;
    Ok(event)
}

fn record_steer(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
    record: &SteerRequestRecord,
) -> Result<i64, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let ordinal: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM turn_inputs
         WHERE agent_id = ?1 AND turn_id = ?2",
        params![agent_id, turn_id],
        |row| row.get(0),
    )?;
    insert_input(&transaction, agent_id, turn_id, ordinal, &record.content)?;
    if let Some(key) = record.idempotency_key.as_deref() {
        insert_request(
            &transaction,
            agent_id,
            key,
            &record.response,
            &record.request_hash,
            record.payment_receipt.as_deref(),
        )?;
    }
    transaction.commit()?;
    Ok(ordinal)
}

fn undo_steer(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
    ordinal: i64,
    idempotency_key: Option<&str>,
) -> Result<(), SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let removed = transaction.execute(
        "DELETE FROM turn_inputs
         WHERE agent_id = ?1 AND turn_id = ?2 AND ordinal = ?3",
        params![agent_id, turn_id, ordinal],
    )?;
    if removed != 1 {
        return Err(SessionError::InvalidState(
            "durable steering input disappeared before compensation",
        ));
    }
    if let Some(key) = idempotency_key {
        transaction.execute(
            "DELETE FROM turn_requests WHERE agent_id = ?1 AND idempotency_key = ?2",
            params![agent_id, key],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

fn mark_started(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "UPDATE turns SET state = 'running', attempt = attempt + 1
         WHERE agent_id = ?1 AND id = ?2",
        params![agent_id, turn_id],
    )?;
    let event = append_event_tx(
        &transaction,
        agent_id,
        Some(turn_id),
        AgentEventPayload::TurnStarted {
            state: TurnStatus::Running,
        },
    )?;
    transaction.commit()?;
    Ok(event)
}

fn request_cancel(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
) -> Result<AgentEvent, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "UPDATE turns SET cancel_requested = 1 WHERE agent_id = ?1 AND id = ?2",
        params![agent_id, turn_id],
    )?;
    let event = append_event_tx(
        &transaction,
        agent_id,
        Some(turn_id),
        AgentEventPayload::TurnCancelRequested,
    )?;
    transaction.commit()?;
    Ok(event)
}

struct EncodedCheckpoint {
    base_ordinal: Option<i64>,
    prefix: i64,
    suffix: i64,
    data: Vec<u8>,
}

fn encode_checkpoint(
    connection: &Connection,
    agent_id: &str,
    ordinal: i64,
    snapshot: &SessionSnapshot,
) -> Result<EncodedCheckpoint, SessionError> {
    let encoded = serde_json::to_vec(snapshot)?;
    let checkpoints_since_full = connection.query_row(
        "SELECT COUNT(*) FROM turns
         WHERE agent_id = ?1 AND ordinal < ?2 AND state = 'completed'
           AND (checkpoint_data IS NOT NULL OR checkpoint_json IS NOT NULL)
           AND ordinal > COALESCE((
               SELECT MAX(ordinal) FROM turns
               WHERE agent_id = ?1 AND ordinal < ?2 AND state = 'completed'
                 AND (checkpoint_json IS NOT NULL OR
                      (checkpoint_data IS NOT NULL AND checkpoint_base_ordinal IS NULL))
           ), 0)",
        params![agent_id, ordinal],
        |row| row.get::<_, i64>(0),
    )?;
    if checkpoints_since_full >= FULL_CHECKPOINT_INTERVAL - 1 {
        return Ok(full_checkpoint(encoded));
    }
    let previous_ordinal = connection
        .query_row(
            "SELECT ordinal FROM turns
             WHERE agent_id = ?1 AND ordinal < ?2
               AND state = 'completed'
               AND (checkpoint_data IS NOT NULL OR checkpoint_json IS NOT NULL)
             ORDER BY ordinal DESC LIMIT 1",
            params![agent_id, ordinal],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(previous_ordinal) = previous_ordinal else {
        return Ok(full_checkpoint(encoded));
    };
    let Some(previous) = reconstruct_checkpoint(connection, agent_id, Some(previous_ordinal))?
    else {
        return Ok(full_checkpoint(encoded));
    };
    let prefix = previous
        .iter()
        .zip(&encoded)
        .take_while(|(left, right)| left == right)
        .count();
    let max_suffix = previous.len().min(encoded.len()).saturating_sub(prefix);
    let suffix = previous
        .iter()
        .rev()
        .zip(encoded.iter().rev())
        .take(max_suffix)
        .take_while(|(left, right)| left == right)
        .count();
    let delta = encoded[prefix..encoded.len() - suffix].to_vec();
    if delta.len().saturating_add(24) >= encoded.len() {
        return Ok(full_checkpoint(encoded));
    }
    Ok(EncodedCheckpoint {
        base_ordinal: Some(previous_ordinal),
        prefix: i64::try_from(prefix).map_err(|_| SessionError::CheckpointOverflow)?,
        suffix: i64::try_from(suffix).map_err(|_| SessionError::CheckpointOverflow)?,
        data: delta,
    })
}

const fn full_checkpoint(data: Vec<u8>) -> EncodedCheckpoint {
    EncodedCheckpoint {
        base_ordinal: None,
        prefix: 0,
        suffix: 0,
        data,
    }
}

fn reconstruct_checkpoint(
    connection: &Connection,
    agent_id: &str,
    through_ordinal: Option<i64>,
) -> Result<Option<Vec<u8>>, SessionError> {
    let start_ordinal = connection
        .query_row(
            "SELECT ordinal FROM turns
             WHERE agent_id = ?1 AND state = 'completed'
               AND (?2 IS NULL OR ordinal <= ?2)
               AND (checkpoint_json IS NOT NULL OR
                    (checkpoint_data IS NOT NULL AND checkpoint_base_ordinal IS NULL))
             ORDER BY ordinal DESC LIMIT 1",
            params![agent_id, through_ordinal],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(start_ordinal) = start_ordinal else {
        return Ok(None);
    };
    let mut statement = connection.prepare(
        "SELECT ordinal, checkpoint_json, checkpoint_base_ordinal,
                checkpoint_prefix, checkpoint_suffix, checkpoint_data
         FROM turns
         WHERE agent_id = ?1 AND state = 'completed'
           AND (?2 IS NULL OR ordinal <= ?2)
           AND ordinal >= ?3
           AND (checkpoint_data IS NOT NULL OR checkpoint_json IS NOT NULL)
         ORDER BY ordinal",
    )?;
    let rows = statement.query_map(params![agent_id, through_ordinal, start_ordinal], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<Vec<u8>>>(5)?,
        ))
    })?;
    let mut current: Option<Vec<u8>> = None;
    let mut current_ordinal = None;
    for row in rows {
        let (ordinal, legacy, base, prefix, suffix, data) = row?;
        let next = if let Some(data) = data {
            if let Some(base) = base {
                if current_ordinal != Some(base) {
                    return Err(SessionError::InvalidState(
                        "checkpoint delta base is not contiguous",
                    ));
                }
                let previous = current.as_deref().ok_or(SessionError::InvalidState(
                    "checkpoint delta is missing its base",
                ))?;
                let prefix = usize::try_from(prefix.unwrap_or_default())
                    .map_err(|_| SessionError::CheckpointOverflow)?;
                let suffix = usize::try_from(suffix.unwrap_or_default())
                    .map_err(|_| SessionError::CheckpointOverflow)?;
                if prefix > previous.len() || suffix > previous.len().saturating_sub(prefix) {
                    return Err(SessionError::InvalidState(
                        "checkpoint delta exceeds its base",
                    ));
                }
                let mut decoded = Vec::with_capacity(prefix + data.len() + suffix);
                decoded.extend_from_slice(&previous[..prefix]);
                decoded.extend_from_slice(&data);
                decoded.extend_from_slice(&previous[previous.len() - suffix..]);
                decoded
            } else {
                data
            }
        } else {
            legacy
                .ok_or(SessionError::InvalidState("checkpoint payload is missing"))?
                .into_bytes()
        };
        current = Some(next);
        current_ordinal = Some(ordinal);
    }
    Ok(current)
}

fn finish_turn(
    connection: &Connection,
    agent_id: &str,
    turn_id: &str,
    completed: CompletedTurn,
) -> Result<FinishedTurn, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let ordinal = transaction.query_row(
        "SELECT ordinal FROM turns WHERE agent_id = ?1 AND id = ?2",
        params![agent_id, turn_id],
        |row| row.get::<_, i64>(0),
    )?;
    let checkpoint = completed
        .snapshot
        .as_ref()
        .map(|snapshot| encode_checkpoint(&transaction, agent_id, ordinal, snapshot))
        .transpose()?;
    let event = append_event_tx(&transaction, agent_id, Some(turn_id), completed.event)?;
    transaction.execute(
        "UPDATE turns
         SET state = ?3, output_json = ?4, usage_json = ?5, error = ?6,
             completed_at = ?7, checkpoint_json = NULL,
             checkpoint_base_ordinal = ?8, checkpoint_prefix = ?9,
             checkpoint_suffix = ?10, checkpoint_data = ?11,
             completion_event_id = ?12
         WHERE agent_id = ?1 AND id = ?2",
        params![
            agent_id,
            turn_id,
            status_name(completed.status),
            serde_json::to_string(&completed.output)?,
            completed
                .usage
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            completed.error,
            completed.completed_at.to_rfc3339(),
            checkpoint
                .as_ref()
                .and_then(|checkpoint| checkpoint.base_ordinal),
            checkpoint
                .as_ref()
                .map_or(0, |checkpoint| checkpoint.prefix),
            checkpoint
                .as_ref()
                .map_or(0, |checkpoint| checkpoint.suffix),
            checkpoint
                .as_ref()
                .map(|checkpoint| checkpoint.data.as_slice()),
            event_id_to_sql(event.id)?,
        ],
    )?;
    transaction.commit()?;
    Ok(FinishedTurn {
        event,
        snapshot: completed.snapshot,
    })
}

fn append_events(
    connection: &Connection,
    agent_id: &str,
    events: Vec<(Option<String>, AgentEventPayload)>,
) -> Result<Vec<AgentEvent>, SessionError> {
    if events.is_empty() {
        return Ok(Vec::new());
    }
    let transaction = connection.unchecked_transaction()?;
    let mut next_id: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM session_events WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;
    let mut persisted = Vec::with_capacity(events.len());
    for (turn_id, payload) in events {
        let id = event_id_from_sql(next_id)?;
        persisted.push(insert_event_tx(
            &transaction,
            agent_id,
            turn_id.as_deref(),
            id,
            payload,
        )?);
        next_id = next_id
            .checked_add(1)
            .ok_or(SessionError::EventIdOverflow)?;
    }
    transaction.commit()?;
    Ok(persisted)
}

fn append_event_tx(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: Option<&str>,
    payload: AgentEventPayload,
) -> Result<AgentEvent, SessionError> {
    let id: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(id), 0) + 1 FROM session_events WHERE agent_id = ?1",
        params![agent_id],
        |row| row.get(0),
    )?;
    let id = event_id_from_sql(id)?;
    insert_event_tx(transaction, agent_id, turn_id, id, payload)
}

fn insert_event_tx(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: Option<&str>,
    id: u64,
    payload: AgentEventPayload,
) -> Result<AgentEvent, SessionError> {
    let created_at = Utc::now();
    transaction.execute(
        "INSERT INTO session_events
         (agent_id, id, turn_id, payload_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            agent_id,
            event_id_to_sql(id)?,
            turn_id,
            serde_json::to_string(&payload)?,
            created_at.to_rfc3339(),
        ],
    )?;
    Ok(AgentEvent {
        id,
        agent_id: agent_id.to_owned(),
        turn_id: turn_id.map(str::to_owned),
        payload,
        created_at,
    })
}

fn events_after(
    connection: &Connection,
    agent_id: &str,
    after_event_id: u64,
    limit: usize,
) -> Result<Vec<AgentEvent>, SessionError> {
    let mut statement = connection.prepare(
        "SELECT id, turn_id, payload_json, created_at
         FROM session_events
         WHERE agent_id = ?1 AND id > ?2
         ORDER BY id LIMIT ?3",
    )?;
    statement
        .query_map(
            params![
                agent_id,
                event_id_to_sql(after_event_id)?,
                i64::try_from(limit).map_err(|_| SessionError::EventIdOverflow)?
            ],
            |row| {
                let payload_json: String = row.get(2)?;
                let created_at: String = row.get(3)?;
                Ok(AgentEvent {
                    id: event_id_from_sql(row.get(0)?)?,
                    agent_id: agent_id.to_owned(),
                    turn_id: row.get(1)?,
                    payload: json_from_sql(&payload_json)?,
                    created_at: parse_time(&created_at)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn fork(
    connection: &Connection,
    source_agent_id: &str,
    target_agent_id: &str,
    selected_turn_id: Option<&str>,
) -> Result<ForkSource, SessionError> {
    let transaction = connection.unchecked_transaction()?;
    let selected = match selected_turn_id {
        Some(turn_id) => transaction
            .query_row(
                "SELECT id, ordinal, completion_event_id FROM turns
                 WHERE agent_id = ?1 AND id = ?2 AND state = 'completed'",
                params![source_agent_id, turn_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?,
        None => transaction
            .query_row(
                "SELECT id, ordinal, completion_event_id FROM turns
                 WHERE agent_id = ?1 AND state = 'completed'
                 ORDER BY ordinal DESC LIMIT 1",
                params![source_agent_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()?,
    }
    .ok_or(SessionError::ForkBoundaryNotFound)?;
    ensure_agent_tx(&transaction, target_agent_id)?;
    transaction.execute(
        "INSERT INTO turns
         (agent_id, id, ordinal, delivery, state, output_json, usage_json, error,
          cancel_requested, attempt, checkpoint_json, checkpoint_base_ordinal,
          checkpoint_prefix, checkpoint_suffix, checkpoint_data,
          completion_event_id, created_at, completed_at)
         SELECT ?1, id, ordinal, delivery, state, output_json, usage_json, error,
                cancel_requested, attempt, checkpoint_json, checkpoint_base_ordinal,
                checkpoint_prefix, checkpoint_suffix, checkpoint_data,
                completion_event_id, created_at, completed_at
         FROM turns WHERE agent_id = ?2 AND ordinal <= ?3",
        params![target_agent_id, source_agent_id, selected.1],
    )?;
    transaction.execute(
        "INSERT INTO turn_inputs
         (agent_id, turn_id, ordinal, content_json, created_at)
         SELECT ?1, turn_id, ordinal, content_json, created_at
         FROM turn_inputs
         WHERE agent_id = ?2
           AND turn_id IN (
               SELECT id FROM turns WHERE agent_id = ?2 AND ordinal <= ?3
           )",
        params![target_agent_id, source_agent_id, selected.1],
    )?;
    transaction.execute(
        "INSERT INTO session_events
         (agent_id, id, turn_id, payload_json, created_at)
         SELECT ?1, id, turn_id, payload_json, created_at
         FROM session_events WHERE agent_id = ?2 AND id <= ?3",
        params![target_agent_id, source_agent_id, selected.2],
    )?;
    transaction.execute(
        "INSERT INTO session_forks
         (agent_id, source_agent_id, source_turn_id, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![target_agent_id, source_agent_id, selected.0, now()],
    )?;
    transaction.commit()?;
    Ok(ForkSource {
        agent_id: source_agent_id.to_owned(),
        turn_id: Some(selected.0),
    })
}

fn delete(connection: &Connection, agent_id: &str) -> Result<(), SessionError> {
    let transaction = connection.unchecked_transaction()?;
    transaction.execute(
        "INSERT OR IGNORE INTO session_tombstones (agent_id, deleted_at) VALUES (?1, ?2)",
        params![agent_id, now()],
    )?;
    transaction.execute(
        "DELETE FROM session_agents WHERE id = ?1",
        params![agent_id],
    )?;
    transaction.commit()?;
    Ok(())
}

fn ensure_agent_tx(transaction: &Transaction<'_>, agent_id: &str) -> Result<(), SessionError> {
    reject_tombstone(transaction, agent_id)?;
    transaction.execute(
        "INSERT OR IGNORE INTO session_agents (id, created_at) VALUES (?1, ?2)",
        params![agent_id, now()],
    )?;
    Ok(())
}

fn reject_tombstone(connection: &Connection, agent_id: &str) -> Result<(), SessionError> {
    let deleted = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM session_tombstones WHERE agent_id = ?1)",
        params![agent_id],
        |row| row.get::<_, bool>(0),
    )?;
    if deleted {
        Err(SessionError::Deleted)
    } else {
        Ok(())
    }
}

fn insert_input(
    transaction: &Transaction<'_>,
    agent_id: &str,
    turn_id: &str,
    ordinal: i64,
    content: &[ContentBlock],
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO turn_inputs
         (agent_id, turn_id, ordinal, content_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            agent_id,
            turn_id,
            ordinal,
            serde_json::to_string(content)?,
            now(),
        ],
    )?;
    Ok(())
}

fn insert_request(
    transaction: &Transaction<'_>,
    agent_id: &str,
    key: &str,
    response: &TurnActionResponse,
    request_hash: &[u8],
    payment_receipt: Option<&str>,
) -> Result<(), SessionError> {
    transaction.execute(
        "INSERT INTO turn_requests
         (agent_id, idempotency_key, action, turn_id, state, request_hash,
          payment_receipt, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            agent_id,
            key,
            action_name(response.action),
            response.turn_id,
            status_name(response.state),
            request_hash,
            payment_receipt,
            now(),
        ],
    )?;
    Ok(())
}

const fn status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Queued => "queued",
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Cancelled => "cancelled",
    }
}

fn parse_status(value: &str) -> rusqlite::Result<TurnStatus> {
    match value {
        "queued" => Ok(TurnStatus::Queued),
        "running" => Ok(TurnStatus::Running),
        "completed" => Ok(TurnStatus::Completed),
        "failed" => Ok(TurnStatus::Failed),
        "cancelled" => Ok(TurnStatus::Cancelled),
        _ => Err(invalid_sql("turn status")),
    }
}

const fn delivery_name(delivery: TurnDelivery) -> &'static str {
    match delivery {
        TurnDelivery::Steer => "steer",
        TurnDelivery::Enqueue => "enqueue",
    }
}

const fn action_name(action: TurnAction) -> &'static str {
    match action {
        TurnAction::Started => "started",
        TurnAction::Steered => "steered",
        TurnAction::Queued => "queued",
    }
}

fn parse_action(value: &str) -> rusqlite::Result<TurnAction> {
    match value {
        "started" => Ok(TurnAction::Started),
        "steered" => Ok(TurnAction::Steered),
        "queued" => Ok(TurnAction::Queued),
        _ => Err(invalid_sql("turn action")),
    }
}

fn parse_time(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|time| time.with_timezone(&Utc))
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

fn json_from_sql<T: serde::de::DeserializeOwned>(value: &str) -> rusqlite::Result<T> {
    serde_json::from_str(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn invalid_sql(name: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        format!("invalid {name}").into(),
    )
}

fn event_id_to_sql(value: u64) -> Result<i64, SessionError> {
    i64::try_from(value).map_err(|_| SessionError::EventIdOverflow)
}

fn event_id_from_sql(value: i64) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|_| invalid_sql("event id"))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

#[derive(Debug, Error)]
pub(crate) enum SessionError {
    #[error("session SQLite actor stopped")]
    Stopped,
    #[error("completed fork boundary was not found")]
    ForkBoundaryNotFound,
    #[error("managed session was permanently deleted")]
    Deleted,
    #[error("session event identifier exceeded SQLite's signed integer range")]
    EventIdOverflow,
    #[error("session checkpoint size exceeded supported integer bounds")]
    CheckpointOverflow,
    #[error("invalid durable session state: {0}")]
    InvalidState(&'static str),
    #[error("session SQLite failed")]
    Database(#[from] rusqlite::Error),
    #[error("session JSON encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("session filesystem setup failed")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_turn_table_gains_nullable_usage_without_rebuild() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                r"
                CREATE TABLE session_agents (
                    id TEXT PRIMARY KEY,
                    created_at TEXT NOT NULL
                );
                CREATE TABLE turns (
                    agent_id TEXT NOT NULL REFERENCES session_agents(id) ON DELETE CASCADE,
                    id TEXT NOT NULL,
                    ordinal INTEGER NOT NULL,
                    delivery TEXT NOT NULL,
                    state TEXT NOT NULL,
                    output_json TEXT NOT NULL DEFAULT '[]',
                    error TEXT,
                    cancel_requested INTEGER NOT NULL DEFAULT 0,
                    attempt INTEGER NOT NULL DEFAULT 0,
                    checkpoint_json TEXT,
                    completion_event_id INTEGER,
                    created_at TEXT NOT NULL,
                    completed_at TEXT,
                    PRIMARY KEY(agent_id, id),
                    UNIQUE(agent_id, ordinal)
                );
                ",
            )
            .unwrap();

        configure(&connection).unwrap();

        let mut statement = connection.prepare("PRAGMA table_info(turns)").unwrap();
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(columns.iter().any(|column| column == "usage_json"));
    }

    #[test]
    fn checkpoint_deltas_reconstruct_shared_history_without_repeating_prefixes() {
        let connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        ensure_agent(&connection, "agent").unwrap();
        let first = br#"{"history":["a"]}"#.to_vec();
        let second = br#"{"history":["a","b"]}"#.to_vec();
        let prefix = first
            .iter()
            .zip(&second)
            .take_while(|(left, right)| left == right)
            .count();
        let suffix = first
            .iter()
            .rev()
            .zip(second.iter().rev())
            .take(first.len().min(second.len()).saturating_sub(prefix))
            .take_while(|(left, right)| left == right)
            .count();
        let delta = &second[prefix..second.len() - suffix];
        for ordinal in 1..=2_i64 {
            connection
                .execute(
                    "INSERT INTO turns
                     (agent_id, id, ordinal, delivery, state, output_json,
                      cancel_requested, attempt, created_at)
                     VALUES ('agent', ?1, ?2, 'steer', 'completed', '[]', 0, 1, ?3)",
                    params![format!("turn-{ordinal}"), ordinal, now()],
                )
                .unwrap();
        }
        connection
            .execute(
                "UPDATE turns SET checkpoint_data = ?1,
                 checkpoint_prefix = 0, checkpoint_suffix = 0 WHERE ordinal = 1",
                params![first],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE turns SET checkpoint_base_ordinal = 1,
                 checkpoint_prefix = ?1, checkpoint_suffix = ?2,
                 checkpoint_data = ?3 WHERE ordinal = 2",
                params![
                    i64::try_from(prefix).unwrap(),
                    i64::try_from(suffix).unwrap(),
                    delta
                ],
            )
            .unwrap();
        assert_eq!(
            reconstruct_checkpoint(&connection, "agent", None)
                .unwrap()
                .unwrap(),
            second
        );
        assert!(delta.len() < second.len());
    }

    #[test]
    fn deleted_session_ids_cannot_be_recreated_by_stale_authorization() {
        let connection = Connection::open_in_memory().unwrap();
        configure(&connection).unwrap();
        ensure_agent(&connection, "deleted-agent").unwrap();
        delete(&connection, "deleted-agent").unwrap();
        assert!(matches!(
            ensure_agent(&connection, "deleted-agent"),
            Err(SessionError::Deleted)
        ));
    }
}
