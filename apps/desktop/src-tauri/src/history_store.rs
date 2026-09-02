use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const MAX_TURNS: usize = 512;
const MAX_ID_BYTES: usize = 128;
const MAX_TURN_ID_BYTES: usize = 64;
const MAX_TITLE_BYTES: usize = 256;
const MAX_TRANSCRIPT_BYTES: usize = 64 * 1024;
const MAX_RESPONSE_BYTES: usize = 256 * 1024;
const MAX_FAILURE_BYTES: usize = 4 * 1024;
const MAX_CONTEXT_EXCHANGES: usize = 16;
const MAX_CONTEXT_BYTES: usize = 32 * 1024;
const MAX_CONTEXT_MESSAGE_BYTES: usize = 16 * 1024;
const NO_ELIGIBLE_CONTEXT: &str = "This Session has no completed exchanges to continue.";
const LATEST_EXCHANGE_TOO_LARGE: &str =
    "The latest exchange is too large to continue without shortening or compression.";
const INVALID_CONTINUATION_WRITE: &str = "conversation history continuation data is invalid";

#[derive(Debug)]
pub struct HistoryStoreError(&'static str);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoryStoreErrorKind {
    Other,
    ContinuationTooLarge,
}

impl HistoryStoreError {
    pub fn kind(&self) -> HistoryStoreErrorKind {
        if self.0 == LATEST_EXCHANGE_TOO_LARGE {
            HistoryStoreErrorKind::ContinuationTooLarge
        } else {
            HistoryStoreErrorKind::Other
        }
    }
}

impl fmt::Display for HistoryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl std::error::Error for HistoryStoreError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryRevision(u64);

impl HistoryRevision {
    pub fn new(value: u64) -> Result<Self, HistoryStoreError> {
        if value == 0 || i64::try_from(value).is_err() {
            return Err(HistoryStoreError(
                "conversation history revision is invalid",
            ));
        }
        Ok(Self(value))
    }

    pub fn get(self) -> u64 {
        self.0
    }

    fn from_database(value: i64) -> Result<Self, HistoryStoreError> {
        let value = u64::try_from(value)
            .map_err(|_| HistoryStoreError("conversation history revision is invalid"))?;
        Self::new(value)
    }

    fn database_value(self) -> Result<i64, HistoryStoreError> {
        i64::try_from(self.0)
            .map_err(|_| HistoryStoreError("conversation history revision is invalid"))
    }

    fn next(self) -> Result<Self, HistoryStoreError> {
        let value = self.0.checked_add(1).ok_or(HistoryStoreError(
            "conversation history revision is invalid",
        ))?;
        Self::new(value)
    }
}

impl Default for HistoryRevision {
    fn default() -> Self {
        Self(1)
    }
}

impl Serialize for HistoryRevision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for HistoryRevision {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let bytes = value.as_bytes();
        if !matches!(bytes.first(), Some(b'1'..=b'9')) || !bytes[1..].iter().all(u8::is_ascii_digit)
        {
            return Err(serde::de::Error::custom(
                "conversation history revision is invalid",
            ));
        }
        let value = value
            .parse::<u64>()
            .map_err(|_| serde::de::Error::custom("conversation history revision is invalid"))?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversationOrigin {
    ContinuedContext,
    #[default]
    Live,
}

impl ConversationOrigin {
    fn as_str(self) -> &'static str {
        match self {
            Self::ContinuedContext => "continued_context",
            Self::Live => "live",
        }
    }

    fn parse(value: &str) -> Result<Self, HistoryStoreError> {
        match value {
            "continued_context" => Ok(Self::ContinuedContext),
            "live" => Ok(Self::Live),
            _ => Err(HistoryStoreError(
                "conversation history contains an invalid turn origin",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContinuationState {
    Preparing,
    Confirmed,
    Unconfirmed,
}

impl ContinuationState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Confirmed => "confirmed",
            Self::Unconfirmed => "unconfirmed",
        }
    }

    fn parse(value: &str) -> Result<Self, HistoryStoreError> {
        match value {
            "preparing" => Ok(Self::Preparing),
            "confirmed" => Ok(Self::Confirmed),
            "unconfirmed" => Ok(Self::Unconfirmed),
            _ => Err(HistoryStoreError(
                "conversation history contains an invalid continuation state",
            )),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationHistory {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(default)]
    pub revision: HistoryRevision,
    #[serde(default)]
    pub continued_from_id: Option<String>,
    #[serde(default)]
    pub continuation_operation_id: Option<String>,
    #[serde(default)]
    pub continuation_state: Option<ContinuationState>,
    pub turns: Vec<ConversationHistoryTurn>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationContextExchange {
    pub user: String,
    pub assistant: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreparedContinuation {
    pub branch: ConversationHistory,
    pub seed: Vec<ConversationContextExchange>,
    pub operation_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub revision: HistoryRevision,
    pub continued_from_id: Option<String>,
    pub continuation_operation_id: Option<String>,
    pub continuation_state: Option<ContinuationState>,
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
    #[serde(default)]
    pub origin: ConversationOrigin,
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

#[derive(Debug)]
pub struct ConversationHistoryStore {
    database: PathBuf,
    connection: Mutex<Connection>,
}

impl ConversationHistoryStore {
    pub fn open(database: &Path) -> Result<Self, HistoryStoreError> {
        let parent = database
            .parent()
            .ok_or(HistoryStoreError("conversation history path is invalid"))?;
        fs::create_dir_all(parent).map_err(|_| {
            HistoryStoreError("conversation history directory could not be created")
        })?;
        let mut connection = open_connection(database)?;
        migrate_schema(&mut connection)?;
        set_private_permissions(database)?;
        Ok(Self {
            database: database.to_path_buf(),
            connection: Mutex::new(connection),
        })
    }

    pub fn database_path(&self) -> &Path {
        &self.database
    }

    pub fn list(&self) -> Result<Vec<ConversationSummary>, HistoryStoreError> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, title, created_at_ms, updated_at_ms, revision,
                        continued_from_id, continuation_operation_id, continuation_state
                 FROM conversations
                 ORDER BY updated_at_ms DESC, id ASC",
            )
            .map_err(database_error)?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(database_error)?;
        rows.map(|row| {
            let (
                id,
                title,
                created_at_ms,
                updated_at_ms,
                revision,
                continued_from_id,
                continuation_operation_id,
                continuation_state,
            ) = row.map_err(database_error)?;
            Ok(ConversationSummary {
                id,
                title,
                created_at_ms,
                updated_at_ms,
                revision: HistoryRevision::from_database(revision)?,
                continued_from_id,
                continuation_operation_id,
                continuation_state: parse_optional_continuation_state(continuation_state)?,
            })
        })
        .collect()
    }

    pub fn get(&self, id: &str) -> Result<Option<ConversationHistory>, HistoryStoreError> {
        validate_text(id, MAX_ID_BYTES, "conversation history id is invalid")?;
        let connection = self.connection()?;
        load_conversation(&connection, id)
    }

    pub fn save_revisioned(
        &self,
        conversation: &ConversationHistory,
        expected_revision: Option<HistoryRevision>,
    ) -> Result<HistoryRevision, HistoryStoreError> {
        validate_conversation(conversation)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let revision = match expected_revision {
            None => {
                validate_revisioned_insert(conversation)?;
                let revision = HistoryRevision::default();
                let changed = transaction
                    .execute(
                        "INSERT INTO conversations
                         (id, title, created_at_ms, updated_at_ms, revision)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(id) DO NOTHING",
                        params![
                            conversation.id,
                            conversation.title,
                            conversation.created_at_ms,
                            conversation.updated_at_ms,
                            revision.database_value()?,
                        ],
                    )
                    .map_err(database_error)?;
                if changed == 0 {
                    return Err(HistoryStoreError("conversation history revision conflict"));
                }
                revision
            }
            Some(expected) => {
                let Some(canonical) = load_conversation(&transaction, &conversation.id)? else {
                    return Err(HistoryStoreError("conversation history revision conflict"));
                };
                if canonical.revision != expected {
                    return Err(HistoryStoreError("conversation history revision conflict"));
                }
                validate_revisioned_update(&canonical, conversation)?;
                let revision = expected.next()?;
                let changed = transaction
                    .execute(
                        "UPDATE conversations
                         SET title = ?2, updated_at_ms = ?3, revision = ?4
                         WHERE id = ?1 AND revision = ?5",
                        params![
                            conversation.id,
                            conversation.title,
                            conversation.updated_at_ms,
                            revision.database_value()?,
                            expected.database_value()?,
                        ],
                    )
                    .map_err(database_error)?;
                if changed == 0 {
                    return Err(HistoryStoreError("conversation history revision conflict"));
                }
                revision
            }
        };
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
                     (conversation_id, position, turn_id, transcript, response, state,
                      failure_message, origin)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                        turn.failure_message,
                        turn.origin.as_str(),
                    ])
                    .map_err(database_error)?;
            }
        }
        transaction.commit().map_err(database_error)?;
        Ok(revision)
    }

    pub fn prepare_continuation(
        &self,
        source_id: &str,
        expected_revision: HistoryRevision,
        now_ms: i64,
        branch_id: &str,
        operation_id: &str,
    ) -> Result<PreparedContinuation, HistoryStoreError> {
        validate_text(
            source_id,
            MAX_ID_BYTES,
            "conversation history id is invalid",
        )?;
        validate_text(
            branch_id,
            MAX_ID_BYTES,
            "conversation history id is invalid",
        )?;
        validate_text(
            operation_id,
            MAX_ID_BYTES,
            "conversation continuation operation id is invalid",
        )?;

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let Some(source) = load_conversation(&transaction, source_id)? else {
            return Err(HistoryStoreError("conversation history was not found"));
        };
        if source.revision != expected_revision {
            return Err(HistoryStoreError("conversation history revision conflict"));
        }

        let seed = select_continuation_seed(&source.turns)?;
        let branch = ConversationHistory {
            id: branch_id.to_owned(),
            title: continuation_title(&source.title),
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            revision: HistoryRevision::default(),
            continued_from_id: Some(source_id.to_owned()),
            continuation_operation_id: Some(operation_id.to_owned()),
            continuation_state: Some(ContinuationState::Preparing),
            turns: seed
                .iter()
                .enumerate()
                .map(|(position, exchange)| ConversationHistoryTurn {
                    turn_id: format!("continued-{}", position + 1),
                    transcript: exchange.user.clone(),
                    response: exchange.assistant.clone(),
                    state: TurnState::Completed,
                    failure_message: None,
                    origin: ConversationOrigin::ContinuedContext,
                })
                .collect(),
        };
        validate_conversation(&branch)?;
        insert_new_conversation(&transaction, &branch)?;
        transaction.commit().map_err(database_error)?;

        Ok(PreparedContinuation {
            branch,
            seed,
            operation_id: operation_id.to_owned(),
        })
    }

    pub fn set_continuation_state(
        &self,
        branch_id: &str,
        expected_revision: HistoryRevision,
        state: ContinuationState,
    ) -> Result<HistoryRevision, HistoryStoreError> {
        validate_text(
            branch_id,
            MAX_ID_BYTES,
            "conversation history id is invalid",
        )?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let row = transaction
            .query_row(
                "SELECT revision, continuation_state
                 FROM conversations WHERE id = ?1",
                params![branch_id],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(database_error)?;
        let Some((revision, current_state)) = row else {
            return Err(HistoryStoreError("conversation history was not found"));
        };
        let revision = HistoryRevision::from_database(revision)?;
        let Some(current_state) = parse_optional_continuation_state(current_state)? else {
            return Err(HistoryStoreError(
                "conversation is not a continuation branch",
            ));
        };

        if current_state == state {
            let repeated_previous_revision = expected_revision
                .get()
                .checked_add(1)
                .is_some_and(|value| value == revision.get());
            if revision == expected_revision || repeated_previous_revision {
                transaction.commit().map_err(database_error)?;
                return Ok(revision);
            }
            return Err(HistoryStoreError("conversation history revision conflict"));
        }
        if revision != expected_revision {
            return Err(HistoryStoreError("conversation history revision conflict"));
        }
        let valid_transition = matches!(
            (current_state, state),
            (
                ContinuationState::Preparing,
                ContinuationState::Confirmed | ContinuationState::Unconfirmed
            ) | (ContinuationState::Unconfirmed, ContinuationState::Confirmed)
        );
        if !valid_transition {
            return Err(HistoryStoreError(
                "conversation continuation state transition is invalid",
            ));
        }

        let next_revision = revision.next()?;
        let changed = transaction
            .execute(
                "UPDATE conversations SET continuation_state = ?2, revision = ?3
                 WHERE id = ?1 AND revision = ?4",
                params![
                    branch_id,
                    state.as_str(),
                    next_revision.database_value()?,
                    revision.database_value()?,
                ],
            )
            .map_err(database_error)?;
        if changed != 1 {
            return Err(HistoryStoreError("conversation history revision conflict"));
        }
        transaction.commit().map_err(database_error)?;
        Ok(next_revision)
    }

    pub fn delete_revisioned(
        &self,
        id: &str,
        expected_revision: HistoryRevision,
    ) -> Result<(), HistoryStoreError> {
        validate_text(id, MAX_ID_BYTES, "conversation history id is invalid")?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(database_error)?;
        let changed = transaction
            .execute(
                "DELETE FROM conversations WHERE id = ?1 AND revision = ?2",
                params![id, expected_revision.database_value()?],
            )
            .map_err(database_error)?;
        if changed == 0 {
            let exists = transaction
                .query_row(
                    "SELECT 1 FROM conversations WHERE id = ?1",
                    params![id],
                    |_| Ok(()),
                )
                .optional()
                .map_err(database_error)?
                .is_some();
            return Err(HistoryStoreError(if exists {
                "conversation history revision conflict"
            } else {
                "conversation history was not found"
            }));
        }
        transaction.commit().map_err(database_error)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, HistoryStoreError> {
        self.connection
            .lock()
            .map_err(|_| HistoryStoreError("conversation history connection is unavailable"))
    }
}

fn load_conversation(
    connection: &Connection,
    id: &str,
) -> Result<Option<ConversationHistory>, HistoryStoreError> {
    let summary = connection
        .query_row(
            "SELECT title, created_at_ms, updated_at_ms, revision,
                    continued_from_id, continuation_operation_id, continuation_state
             FROM conversations WHERE id = ?1",
            params![id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(database_error)?;
    let Some((
        title,
        created_at_ms,
        updated_at_ms,
        revision,
        continued_from_id,
        continuation_operation_id,
        continuation_state,
    )) = summary
    else {
        return Ok(None);
    };
    let mut statement = connection
        .prepare(
            "SELECT turn_id, transcript, response, state, failure_message, origin
             FROM conversation_turns
             WHERE conversation_id = ?1
             ORDER BY position ASC",
        )
        .map_err(database_error)?;
    let rows = statement
        .query_map(params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(database_error)?;
    let turns = rows
        .map(|row| {
            let (turn_id, transcript, response, state, failure_message, origin) =
                row.map_err(database_error)?;
            Ok(ConversationHistoryTurn {
                turn_id,
                transcript,
                response,
                state: TurnState::parse(&state)?,
                failure_message,
                origin: ConversationOrigin::parse(&origin)?,
            })
        })
        .collect::<Result<Vec<_>, HistoryStoreError>>()?;
    Ok(Some(ConversationHistory {
        id: id.to_owned(),
        title,
        created_at_ms,
        updated_at_ms,
        revision: HistoryRevision::from_database(revision)?,
        continued_from_id,
        continuation_operation_id,
        continuation_state: parse_optional_continuation_state(continuation_state)?,
        turns,
    }))
}

fn select_continuation_seed(
    turns: &[ConversationHistoryTurn],
) -> Result<Vec<ConversationContextExchange>, HistoryStoreError> {
    let mut selected = Vec::new();
    let mut selected_bytes = 0_usize;
    for turn in turns.iter().rev().filter(|turn| {
        turn.state == TurnState::Completed
            && !turn.transcript.trim().is_empty()
            && !turn.response.trim().is_empty()
    }) {
        if selected.len() == MAX_CONTEXT_EXCHANGES {
            break;
        }
        let pair_bytes = turn
            .transcript
            .len()
            .checked_add(turn.response.len())
            .ok_or(HistoryStoreError(LATEST_EXCHANGE_TOO_LARGE))?;
        let violates_limit = turn.transcript.len() > MAX_CONTEXT_MESSAGE_BYTES
            || turn.response.len() > MAX_CONTEXT_MESSAGE_BYTES
            || selected_bytes
                .checked_add(pair_bytes)
                .is_none_or(|bytes| bytes > MAX_CONTEXT_BYTES);
        if violates_limit {
            if selected.is_empty() {
                return Err(HistoryStoreError(LATEST_EXCHANGE_TOO_LARGE));
            }
            break;
        }
        selected_bytes += pair_bytes;
        selected.push(ConversationContextExchange {
            user: turn.transcript.clone(),
            assistant: turn.response.clone(),
        });
    }
    if selected.is_empty() {
        return Err(HistoryStoreError(NO_ELIGIBLE_CONTEXT));
    }
    selected.reverse();
    Ok(selected)
}

fn continuation_title(source_title: &str) -> String {
    let mut title = format!("Continued: {source_title}");
    if title.len() > MAX_TITLE_BYTES {
        let mut end = MAX_TITLE_BYTES;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
    }
    title
}

fn insert_new_conversation(
    connection: &Connection,
    conversation: &ConversationHistory,
) -> Result<(), HistoryStoreError> {
    let changed = connection
        .execute(
            "INSERT INTO conversations
             (id, title, created_at_ms, updated_at_ms, revision, continued_from_id,
              continuation_operation_id, continuation_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(id) DO NOTHING",
            params![
                conversation.id,
                conversation.title,
                conversation.created_at_ms,
                conversation.updated_at_ms,
                conversation.revision.database_value()?,
                conversation.continued_from_id,
                conversation.continuation_operation_id,
                conversation
                    .continuation_state
                    .map(ContinuationState::as_str),
            ],
        )
        .map_err(database_error)?;
    if changed == 0 {
        return Err(HistoryStoreError("conversation history revision conflict"));
    }
    let mut statement = connection
        .prepare(
            "INSERT INTO conversation_turns
             (conversation_id, position, turn_id, transcript, response, state,
              failure_message, origin)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                turn.failure_message,
                turn.origin.as_str(),
            ])
            .map_err(database_error)?;
    }
    Ok(())
}

fn migrate_schema(connection: &mut Connection) -> Result<(), HistoryStoreError> {
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(database_error)?;
    let version = transaction
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(database_error)?;
    match version {
        0 => {
            let has_conversations = table_exists(&transaction, "conversations")?;
            let has_turns = table_exists(&transaction, "conversation_turns")?;
            match (has_conversations, has_turns) {
                (false, false) => transaction
                    .execute_batch(
                        "CREATE TABLE conversations (
                           id TEXT PRIMARY KEY NOT NULL,
                           title TEXT NOT NULL,
                           created_at_ms INTEGER NOT NULL,
                           updated_at_ms INTEGER NOT NULL,
                           revision INTEGER NOT NULL CHECK (revision > 0),
                           continued_from_id TEXT,
                           continuation_operation_id TEXT,
                           continuation_state TEXT CHECK (
                             continuation_state IS NULL OR
                             continuation_state IN ('preparing', 'confirmed', 'unconfirmed')
                           )
                         );
                         CREATE TABLE conversation_turns (
                           conversation_id TEXT NOT NULL,
                           position INTEGER NOT NULL,
                           turn_id TEXT NOT NULL,
                           transcript TEXT NOT NULL,
                           response TEXT NOT NULL,
                           state TEXT NOT NULL,
                           failure_message TEXT,
                           origin TEXT NOT NULL DEFAULT 'live' CHECK (
                             origin IN ('continued_context', 'live')
                           ),
                           PRIMARY KEY (conversation_id, position),
                           FOREIGN KEY (conversation_id) REFERENCES conversations(id)
                             ON DELETE CASCADE
                         );
                         CREATE INDEX conversations_updated
                           ON conversations(updated_at_ms DESC, id ASC);",
                    )
                    .map_err(database_error)?,
                (true, true) => transaction
                    .execute_batch(
                        "ALTER TABLE conversations
                           ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0);
                         ALTER TABLE conversations ADD COLUMN continued_from_id TEXT;
                         ALTER TABLE conversations ADD COLUMN continuation_operation_id TEXT;
                         ALTER TABLE conversations ADD COLUMN continuation_state TEXT CHECK (
                           continuation_state IS NULL OR
                           continuation_state IN ('preparing', 'confirmed', 'unconfirmed')
                         );
                         ALTER TABLE conversation_turns
                           ADD COLUMN origin TEXT NOT NULL DEFAULT 'live' CHECK (
                             origin IN ('continued_context', 'live')
                           );
                         DROP INDEX IF EXISTS conversations_updated;
                         CREATE INDEX conversations_updated
                           ON conversations(updated_at_ms DESC, id ASC);",
                    )
                    .map_err(database_error)?,
                _ => {
                    return Err(HistoryStoreError(
                        "conversation history database schema is invalid",
                    ));
                }
            }
            transaction
                .pragma_update(None, "user_version", 2_i64)
                .map_err(database_error)?;
        }
        2 => {}
        _ => {
            return Err(HistoryStoreError(
                "conversation history database schema is unsupported",
            ));
        }
    }
    transaction.commit().map_err(database_error)
}

fn table_exists(connection: &Connection, name: &str) -> Result<bool, HistoryStoreError> {
    connection
        .query_row(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            params![name],
            |_| Ok(()),
        )
        .optional()
        .map(|value| value.is_some())
        .map_err(database_error)
}

fn parse_optional_continuation_state(
    value: Option<String>,
) -> Result<Option<ContinuationState>, HistoryStoreError> {
    value.as_deref().map(ContinuationState::parse).transpose()
}

fn open_connection(database: &Path) -> Result<Connection, HistoryStoreError> {
    let connection = Connection::open(database).map_err(database_error)?;
    connection
        .busy_timeout(Duration::from_secs(2))
        .map_err(database_error)?;
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(database_error)?;
    Ok(connection)
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

fn validate_revisioned_insert(value: &ConversationHistory) -> Result<(), HistoryStoreError> {
    if value.continued_from_id.is_some()
        || value.continuation_operation_id.is_some()
        || value.continuation_state.is_some()
        || value
            .turns
            .iter()
            .any(|turn| turn.origin != ConversationOrigin::Live)
    {
        return Err(HistoryStoreError(INVALID_CONTINUATION_WRITE));
    }
    Ok(())
}

fn validate_revisioned_update(
    canonical: &ConversationHistory,
    value: &ConversationHistory,
) -> Result<(), HistoryStoreError> {
    if value.created_at_ms != canonical.created_at_ms {
        return Err(HistoryStoreError(
            "conversation history creation timestamp is immutable",
        ));
    }
    if value.continued_from_id != canonical.continued_from_id
        || value.continuation_operation_id != canonical.continuation_operation_id
        || value.continuation_state != canonical.continuation_state
    {
        return Err(HistoryStoreError(INVALID_CONTINUATION_WRITE));
    }

    let copied_context_len = canonical
        .turns
        .iter()
        .take_while(|turn| turn.origin == ConversationOrigin::ContinuedContext)
        .count();
    if value.turns.len() < copied_context_len
        || value.turns[..copied_context_len] != canonical.turns[..copied_context_len]
        || value.turns[copied_context_len..]
            .iter()
            .any(|turn| turn.origin != ConversationOrigin::Live)
    {
        return Err(HistoryStoreError(INVALID_CONTINUATION_WRITE));
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
