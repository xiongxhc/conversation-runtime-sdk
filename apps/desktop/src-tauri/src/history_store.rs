use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const MAX_TURNS: usize = 512;
const MAX_ID_BYTES: usize = 128;
const MAX_TURN_ID_BYTES: usize = 64;
const MAX_TITLE_BYTES: usize = 256;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_FAILURE_BYTES: usize = 4 * 1024;

#[derive(Debug)]
pub struct HistoryStoreError(&'static str);

impl fmt::Display for HistoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for HistoryStoreError {}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationHistory {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub turns: Vec<ConversationHistoryTurn>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationHistoryTurn {
    pub turn_id: String,
    pub transcript: String,
    pub response: String,
    pub state: TurnState,
    #[serde(default)]
    pub failure_message: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnState {
    Streaming,
    Completed,
    Cancelled,
    Failed,
}

impl TurnState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, HistoryStoreError> {
        match value {
            "streaming" => Ok(Self::Streaming),
            "completed" => Ok(Self::Completed),
            "cancelled" => Ok(Self::Cancelled),
            "failed" => Ok(Self::Failed),
            _ => Err(HistoryStoreError(
                "conversation history contains an invalid turn state",
            )),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ConversationHistoryStore {
    database: PathBuf,
}

impl ConversationHistoryStore {
    pub fn open(database: &Path) -> Result<Self, HistoryStoreError> {
        let parent = database
            .parent()
            .ok_or(HistoryStoreError("conversation history path is invalid"))?;
        fs::create_dir_all(parent).map_err(|_| {
            HistoryStoreError("conversation history directory could not be created")
        })?;
        let store = Self {
            database: database.to_path_buf(),
        };
        let connection = store.connection()?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON;
                 CREATE TABLE IF NOT EXISTS conversations (
                   id TEXT PRIMARY KEY NOT NULL,
                   title TEXT NOT NULL,
                   created_at_ms INTEGER NOT NULL,
                   updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS conversation_turns (
                   conversation_id TEXT NOT NULL,
                   position INTEGER NOT NULL,
                   turn_id TEXT NOT NULL,
                   transcript TEXT NOT NULL,
                   response TEXT NOT NULL,
                   state TEXT NOT NULL,
                   failure_message TEXT,
                   PRIMARY KEY (conversation_id, position),
                   FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
                 );
                 CREATE INDEX IF NOT EXISTS conversations_updated
                   ON conversations(updated_at_ms DESC, id ASC);",
            )
            .map_err(database_error)?;
        set_private_permissions(database)?;
        Ok(store)
    }

    pub fn database_path(&self) -> &Path {
        &self.database
    }

    pub fn list(&self) -> Result<Vec<ConversationSummary>, HistoryStoreError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, title, created_at_ms, updated_at_ms
                 FROM conversations
                 ORDER BY updated_at_ms DESC, id ASC",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok(ConversationSummary {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    created_at_ms: row.get(2)?,
                    updated_at_ms: row.get(3)?,
                })
            })
            .map_err(database_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(database_error)
    }

    pub fn get(&self, id: &str) -> Result<Option<ConversationHistory>, HistoryStoreError> {
        validate_text(id, MAX_ID_BYTES, "conversation history id is invalid")?;
        let connection = self.connection()?;
        let summary = connection
            .query_row(
                "SELECT title, created_at_ms, updated_at_ms FROM conversations WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(database_error)?;
        let Some((title, created_at_ms, updated_at_ms)) = summary else {
            return Ok(None);
        };
        let mut statement = connection
            .prepare(
                "SELECT turn_id, transcript, response, state, failure_message
                 FROM conversation_turns
                 WHERE conversation_id = ?1
                 ORDER BY position ASC",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map(params![id], |row| {
                let state = row.get::<_, String>(3)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    state,
                    row.get::<_, Option<String>>(4)?,
                ))
            })
            .map_err(database_error)?;
        let turns = rows
            .map(|row| {
                let (turn_id, transcript, response, state, failure_message) =
                    row.map_err(database_error)?;
                Ok(ConversationHistoryTurn {
                    turn_id,
                    transcript,
                    response,
                    state: TurnState::parse(&state)?,
                    failure_message,
                })
            })
            .collect::<Result<Vec<_>, HistoryStoreError>>()?;
        Ok(Some(ConversationHistory {
            id: id.to_owned(),
            title,
            created_at_ms,
            updated_at_ms,
            turns,
        }))
    }

    pub fn save(&self, conversation: &ConversationHistory) -> Result<(), HistoryStoreError> {
        validate_conversation(conversation)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        transaction
            .execute(
                "INSERT INTO conversations (id, title, created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                   title = excluded.title,
                   created_at_ms = excluded.created_at_ms,
                   updated_at_ms = excluded.updated_at_ms",
                params![
                    conversation.id,
                    conversation.title,
                    conversation.created_at_ms,
                    conversation.updated_at_ms
                ],
            )
            .map_err(database_error)?;
        transaction
            .execute(
                "DELETE FROM conversation_turns WHERE conversation_id = ?1",
                params![conversation.id],
            )
            .map_err(database_error)?;
        {
            let mut statement = transaction
                .prepare(
                    "INSERT INTO conversation_turns
                     (conversation_id, position, turn_id, transcript, response, state, failure_message)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(database_error)?;
            for (position, turn) in conversation.turns.iter().enumerate() {
                statement
                    .execute(params![
                        conversation.id,
                        position as i64,
                        turn.turn_id,
                        turn.transcript,
                        turn.response,
                        turn.state.as_str(),
                        turn.failure_message
                    ])
                    .map_err(database_error)?;
            }
        }
        transaction.commit().map_err(database_error)
    }

    pub fn delete(&self, id: &str) -> Result<(), HistoryStoreError> {
        validate_text(id, MAX_ID_BYTES, "conversation history id is invalid")?;
        self.connection()?
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])
            .map(|_| ())
            .map_err(database_error)
    }

    fn connection(&self) -> Result<Connection, HistoryStoreError> {
        let connection = Connection::open(&self.database).map_err(database_error)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(database_error)?;
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .map_err(database_error)?;
        Ok(connection)
    }
}

fn validate_conversation(value: &ConversationHistory) -> Result<(), HistoryStoreError> {
    validate_text(
        &value.id,
        MAX_ID_BYTES,
        "conversation history id is invalid",
    )?;
    validate_text(
        &value.title,
        MAX_TITLE_BYTES,
        "conversation history title is invalid",
    )?;
    if value.created_at_ms < 0 || value.updated_at_ms < value.created_at_ms {
        return Err(HistoryStoreError(
            "conversation history timestamps are invalid",
        ));
    }
    if value.turns.is_empty() || value.turns.len() > MAX_TURNS {
        return Err(HistoryStoreError(
            "conversation history turn count is invalid",
        ));
    }
    for turn in &value.turns {
        validate_text(
            &turn.turn_id,
            MAX_TURN_ID_BYTES,
            "conversation history turn id is invalid",
        )?;
        validate_required_content(
            &turn.transcript,
            MAX_TRANSCRIPT_BYTES,
            "conversation history transcript is invalid",
        )?;
        validate_optional_text(
            &turn.response,
            MAX_RESPONSE_BYTES,
            "conversation history response is invalid",
        )?;
        if let Some(message) = turn.failure_message.as_deref() {
            validate_optional_text(
                message,
                MAX_FAILURE_BYTES,
                "conversation history failure is invalid",
            )?;
        }
    }
    Ok(())
}

fn validate_text(
    value: &str,
    maximum: usize,
    message: &'static str,
) -> Result<(), HistoryStoreError> {
    if value.trim().is_empty() || value.len() > maximum || value.chars().any(char::is_control) {
        return Err(HistoryStoreError(message));
    }
    Ok(())
}

fn validate_optional_text(
    value: &str,
    maximum: usize,
    message: &'static str,
) -> Result<(), HistoryStoreError> {
    if value.len() > maximum || value.contains('\0') {
        return Err(HistoryStoreError(message));
    }
    Ok(())
}

fn validate_required_content(
    value: &str,
    maximum: usize,
    message: &'static str,
) -> Result<(), HistoryStoreError> {
    if value.trim().is_empty() || value.len() > maximum || value.contains('\0') {
        return Err(HistoryStoreError(message));
    }
    Ok(())
}

fn database_error(_error: rusqlite::Error) -> HistoryStoreError {
    HistoryStoreError("conversation history database operation failed")
}

#[cfg(unix)]
fn set_private_permissions(path: &Path) -> Result<(), HistoryStoreError> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|_| HistoryStoreError("conversation history permissions could not be secured"))
}

#[cfg(not(unix))]
fn set_private_permissions(_path: &Path) -> Result<(), HistoryStoreError> {
    Ok(())
}
