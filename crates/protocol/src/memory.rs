use std::fmt;

use crate::{RuntimeError, RuntimeErrorKind, RuntimeStage, SessionId, TurnId};
use sha2::{Digest, Sha256};

pub const MAX_MEMORY_CONTENT_BYTES: usize = 4 * 1024;
pub const MAX_MEMORY_RETRIEVAL_ITEMS: usize = 8;
pub const MAX_MEMORY_RETRIEVAL_BYTES: usize = 8 * 1024;
pub const MAX_MEMORY_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_WORKING_RETENTION_MILLIS: i64 = 24 * 60 * 60 * 1_000;

const MAX_SOURCE_IDENTIFIER_BYTES: usize = 512;
const MAX_ACTOR_BYTES: usize = 256;
const MAX_DIGEST_BYTES: usize = 256;
const MAX_CONFIRMATION_IDENTIFIER_BYTES: usize = 512;

macro_rules! checked_id {
    ($name:ident, $label:literal) => {
        #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(u64);

        impl $name {
            pub fn new(value: u64) -> Result<Self, RuntimeError> {
                if value == 0 {
                    return Err(memory_error(concat!($label, " must be non-zero")));
                }
                Ok(Self(value))
            }

            pub const fn get(self) -> u64 {
                self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

checked_id!(MemoryId, "memory identifier");
checked_id!(RetrievalTraceId, "retrieval trace identifier");

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct UnixTimestampMillis(i64);

impl UnixTimestampMillis {
    pub fn new(value: i64) -> Result<Self, RuntimeError> {
        if value < 0 {
            return Err(memory_error("timestamp must be non-negative"));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> i64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MemoryConfidence(u16);

impl MemoryConfidence {
    pub fn new(value: u16) -> Result<Self, RuntimeError> {
        if value > 1_000 {
            return Err(memory_error("memory confidence exceeds 1000"));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u16 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum MemoryKind {
    Working,
    Episodic,
    Semantic,
    Identity,
    Relationship,
}

impl MemoryKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Episodic => "episodic",
            Self::Semantic => "semantic",
            Self::Identity => "identity",
            Self::Relationship => "relationship",
        }
    }

    pub const fn requires_approval(self) -> bool {
        matches!(self, Self::Identity | Self::Relationship)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryState {
    Candidate,
    Active,
    Expired,
}

impl MemoryState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Candidate => "candidate",
            Self::Active => "active",
            Self::Expired => "expired",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryProvenanceKind {
    UserProvided,
    UserEdited,
    CompletedExchange,
    ApplicationImported,
}

impl MemoryProvenanceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UserProvided => "user_provided",
            Self::UserEdited => "user_edited",
            Self::CompletedExchange => "completed_exchange",
            Self::ApplicationImported => "application_imported",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryProvenance {
    kind: MemoryProvenanceKind,
    source_id: String,
    source_timestamp: UnixTimestampMillis,
    actor: String,
    content_digest: Option<String>,
}

impl MemoryProvenance {
    pub fn new(
        kind: MemoryProvenanceKind,
        source_id: impl Into<String>,
        source_timestamp: UnixTimestampMillis,
        actor: impl Into<String>,
        content_digest: Option<String>,
    ) -> Result<Self, RuntimeError> {
        let source_id = bounded_text(
            source_id.into(),
            MAX_SOURCE_IDENTIFIER_BYTES,
            "memory provenance source identifier must not be empty",
            "memory provenance source identifier exceeds 512 bytes",
        )?;
        let actor = bounded_text(
            actor.into(),
            MAX_ACTOR_BYTES,
            "memory provenance actor must not be empty",
            "memory provenance actor exceeds 256 bytes",
        )?;
        let content_digest = content_digest
            .map(|digest| {
                bounded_text(
                    digest,
                    MAX_DIGEST_BYTES,
                    "memory content digest must not be empty",
                    "memory content digest exceeds 256 bytes",
                )
            })
            .transpose()?;
        Ok(Self {
            kind,
            source_id,
            source_timestamp,
            actor,
            content_digest,
        })
    }

    pub const fn kind(&self) -> MemoryProvenanceKind {
        self.kind
    }

    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    pub const fn source_timestamp(&self) -> UnixTimestampMillis {
        self.source_timestamp
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub fn content_digest(&self) -> Option<&str> {
        self.content_digest.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryApproval {
    confirmation_id: String,
    actor: String,
    confirmed_at: UnixTimestampMillis,
    expected_revision: u64,
}

impl MemoryApproval {
    pub fn new(
        confirmation_id: impl Into<String>,
        actor: impl Into<String>,
        confirmed_at: UnixTimestampMillis,
        expected_revision: u64,
    ) -> Result<Self, RuntimeError> {
        let confirmation_id = bounded_text(
            confirmation_id.into(),
            MAX_CONFIRMATION_IDENTIFIER_BYTES,
            "memory confirmation identifier must not be empty",
            "memory confirmation identifier exceeds 512 bytes",
        )?;
        let actor = bounded_text(
            actor.into(),
            MAX_ACTOR_BYTES,
            "memory approval actor must not be empty",
            "memory approval actor exceeds 256 bytes",
        )?;
        validate_revision(expected_revision)?;
        Ok(Self {
            confirmation_id,
            actor,
            confirmed_at,
            expected_revision,
        })
    }

    pub fn confirmation_id(&self) -> &str {
        &self.confirmation_id
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub const fn confirmed_at(&self) -> UnixTimestampMillis {
        self.confirmed_at
    }

    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    pub fn evidence_for(&self, content: &str) -> MemoryApprovalEvidence {
        MemoryApprovalEvidence {
            confirmation_id: self.confirmation_id.clone(),
            actor: self.actor.clone(),
            confirmed_at: self.confirmed_at,
            approved_revision: self.expected_revision,
            content_digest: content_digest(content),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryApprovalEvidence {
    confirmation_id: String,
    actor: String,
    confirmed_at: UnixTimestampMillis,
    approved_revision: u64,
    content_digest: String,
}

impl MemoryApprovalEvidence {
    pub fn confirmation_id(&self) -> &str {
        &self.confirmation_id
    }

    pub fn actor(&self) -> &str {
        &self.actor
    }

    pub const fn confirmed_at(&self) -> UnixTimestampMillis {
        self.confirmed_at
    }

    pub const fn approved_revision(&self) -> u64 {
        self.approved_revision
    }

    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    pub fn matches_content(&self, content: &str) -> bool {
        self.content_digest == content_digest(content)
    }

    pub fn from_stored(
        confirmation_id: impl Into<String>,
        actor: impl Into<String>,
        confirmed_at: UnixTimestampMillis,
        approved_revision: u64,
        content_digest: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let approval =
            MemoryApproval::new(confirmation_id, actor, confirmed_at, approved_revision)?;
        let content_digest = bounded_text(
            content_digest.into(),
            MAX_DIGEST_BYTES,
            "memory approval content digest must not be empty",
            "memory approval content digest exceeds 256 bytes",
        )?;
        if !is_sha256_digest(&content_digest) {
            return Err(memory_error("memory approval content digest is invalid"));
        }
        Ok(Self {
            confirmation_id: approval.confirmation_id,
            actor: approval.actor,
            confirmed_at,
            approved_revision,
            content_digest,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryRetention {
    Working { expires_at: UnixTimestampMillis },
    Session { session_id: SessionId },
    Until { expires_at: UnixTimestampMillis },
    UntilDeleted,
}

impl MemoryRetention {
    pub const fn working(expires_at: UnixTimestampMillis) -> Self {
        Self::Working { expires_at }
    }

    pub const fn session(session_id: SessionId) -> Self {
        Self::Session { session_id }
    }

    pub const fn until(expires_at: UnixTimestampMillis) -> Self {
        Self::Until { expires_at }
    }

    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Working { .. } => "working",
            Self::Session { .. } => "session",
            Self::Until { .. } => "until",
            Self::UntilDeleted => "until_deleted",
        }
    }

    pub const fn expires_at(&self) -> Option<UnixTimestampMillis> {
        match self {
            Self::Working { expires_at } | Self::Until { expires_at } => Some(*expires_at),
            Self::Session { .. } | Self::UntilDeleted => None,
        }
    }

    pub const fn session_id(&self) -> Option<SessionId> {
        match self {
            Self::Session { session_id } => Some(*session_id),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryDraft {
    kind: MemoryKind,
    content: String,
    provenance: MemoryProvenance,
    confidence: MemoryConfidence,
    created_at: UnixTimestampMillis,
    retention: MemoryRetention,
}

impl MemoryDraft {
    pub fn new(
        kind: MemoryKind,
        content: impl Into<String>,
        provenance: MemoryProvenance,
        confidence: MemoryConfidence,
        created_at: UnixTimestampMillis,
        retention: MemoryRetention,
    ) -> Result<Self, RuntimeError> {
        let content = validate_content(content.into())?;
        validate_retention(kind, created_at, &retention)?;
        Ok(Self {
            kind,
            content,
            provenance,
            confidence,
            created_at,
            retention,
        })
    }

    pub const fn kind(&self) -> MemoryKind {
        self.kind
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub const fn provenance(&self) -> &MemoryProvenance {
        &self.provenance
    }

    pub const fn confidence(&self) -> MemoryConfidence {
        self.confidence
    }

    pub const fn created_at(&self) -> UnixTimestampMillis {
        self.created_at
    }

    pub const fn retention(&self) -> &MemoryRetention {
        &self.retention
    }

    pub const fn initial_state(&self) -> MemoryState {
        if self.kind.requires_approval() {
            MemoryState::Candidate
        } else {
            MemoryState::Active
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryPatch {
    expected_revision: u64,
    content: Option<String>,
    confidence: Option<MemoryConfidence>,
    retention: Option<MemoryRetention>,
    edited_at: UnixTimestampMillis,
    provenance: MemoryProvenance,
}

impl MemoryPatch {
    pub fn new(
        expected_revision: u64,
        content: Option<String>,
        confidence: Option<MemoryConfidence>,
        retention: Option<MemoryRetention>,
        edited_at: UnixTimestampMillis,
        provenance: MemoryProvenance,
    ) -> Result<Self, RuntimeError> {
        validate_revision(expected_revision)?;
        if content.is_none() && confidence.is_none() && retention.is_none() {
            return Err(memory_error("memory patch must change at least one field"));
        }
        let content = content.map(validate_content).transpose()?;
        if provenance.kind() != MemoryProvenanceKind::UserEdited {
            return Err(memory_error("memory patch provenance must be user edited"));
        }
        Ok(Self {
            expected_revision,
            content,
            confidence,
            retention,
            edited_at,
            provenance,
        })
    }

    pub const fn expected_revision(&self) -> u64 {
        self.expected_revision
    }

    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
    }

    pub const fn confidence(&self) -> Option<MemoryConfidence> {
        self.confidence
    }

    pub const fn retention(&self) -> Option<&MemoryRetention> {
        self.retention.as_ref()
    }

    pub const fn edited_at(&self) -> UnixTimestampMillis {
        self.edited_at
    }

    pub const fn provenance(&self) -> &MemoryProvenance {
        &self.provenance
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryRecord {
    id: MemoryId,
    draft: MemoryDraft,
    state: MemoryState,
    updated_at: UnixTimestampMillis,
    pinned: bool,
    revision: u64,
    approval: Option<MemoryApprovalEvidence>,
    last_used_at: Option<UnixTimestampMillis>,
    last_retrieval_reason: Option<MemoryRetrievalReason>,
}

impl MemoryRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: MemoryId,
        draft: MemoryDraft,
        state: MemoryState,
        updated_at: UnixTimestampMillis,
        pinned: bool,
        revision: u64,
        approval: Option<MemoryApprovalEvidence>,
        last_used_at: Option<UnixTimestampMillis>,
        last_retrieval_reason: Option<MemoryRetrievalReason>,
    ) -> Result<Self, RuntimeError> {
        validate_revision(revision)?;
        if updated_at < draft.created_at() {
            return Err(memory_error("memory update precedes creation"));
        }
        if last_used_at.is_some_and(|last_used| last_used < draft.created_at()) {
            return Err(memory_error("memory last use precedes creation"));
        }
        if pinned && draft.kind() == MemoryKind::Working {
            return Err(memory_error("working memory cannot be pinned"));
        }
        if approval.as_ref().is_some_and(|evidence| {
            evidence.content_digest() != content_digest(draft.content())
                || evidence.confirmed_at() > updated_at
                || evidence.approved_revision() >= revision
        }) {
            return Err(memory_error("memory approval evidence is invalid"));
        }
        if draft.kind().requires_approval() && state == MemoryState::Active && approval.is_none() {
            return Err(memory_error(
                "active identity or relationship memory requires approval evidence",
            ));
        }
        Ok(Self {
            id,
            draft,
            state,
            updated_at,
            pinned,
            revision,
            approval,
            last_used_at,
            last_retrieval_reason,
        })
    }

    pub const fn id(&self) -> MemoryId {
        self.id
    }

    pub const fn kind(&self) -> MemoryKind {
        self.draft.kind()
    }

    pub fn content(&self) -> &str {
        self.draft.content()
    }

    pub const fn provenance(&self) -> &MemoryProvenance {
        self.draft.provenance()
    }

    pub const fn confidence(&self) -> MemoryConfidence {
        self.draft.confidence()
    }

    pub const fn created_at(&self) -> UnixTimestampMillis {
        self.draft.created_at()
    }

    pub const fn retention(&self) -> &MemoryRetention {
        self.draft.retention()
    }

    pub const fn state(&self) -> MemoryState {
        self.state
    }

    pub const fn updated_at(&self) -> UnixTimestampMillis {
        self.updated_at
    }

    pub const fn pinned(&self) -> bool {
        self.pinned
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub const fn approval(&self) -> Option<&MemoryApprovalEvidence> {
        self.approval.as_ref()
    }

    pub const fn last_used_at(&self) -> Option<UnixTimestampMillis> {
        self.last_used_at
    }

    pub const fn last_retrieval_reason(&self) -> Option<MemoryRetrievalReason> {
        self.last_retrieval_reason
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MemoryRetrievalReason {
    PinnedMatch,
    ExactPhrase,
    SharedTerm,
    RecentWorking,
}

impl MemoryRetrievalReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PinnedMatch => "pinned_match",
            Self::ExactPhrase => "exact_phrase",
            Self::SharedTerm => "shared_term",
            Self::RecentWorking => "recent_working",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryRetrievalRequest {
    turn_id: TurnId,
    query: String,
    now: UnixTimestampMillis,
    maximum_items: usize,
    maximum_bytes: usize,
}

impl MemoryRetrievalRequest {
    pub fn new(
        turn_id: TurnId,
        query: impl Into<String>,
        now: UnixTimestampMillis,
        maximum_items: usize,
        maximum_bytes: usize,
    ) -> Result<Self, RuntimeError> {
        let query = bounded_text(
            query.into(),
            MAX_MEMORY_QUERY_BYTES,
            "memory retrieval query must not be empty",
            "memory retrieval query exceeds 16 KiB",
        )?;
        if !(1..=MAX_MEMORY_RETRIEVAL_ITEMS).contains(&maximum_items) {
            return Err(memory_error(
                "memory retrieval item limit must be 1 through 8",
            ));
        }
        if !(1..=MAX_MEMORY_RETRIEVAL_BYTES).contains(&maximum_bytes) {
            return Err(memory_error(
                "memory retrieval byte limit must be 1 through 8192",
            ));
        }
        Ok(Self {
            turn_id,
            query,
            now,
            maximum_items,
            maximum_bytes,
        })
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    pub const fn now(&self) -> UnixTimestampMillis {
        self.now
    }

    pub const fn maximum_items(&self) -> usize {
        self.maximum_items
    }

    pub const fn maximum_bytes(&self) -> usize {
        self.maximum_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryContextItem {
    memory_id: MemoryId,
    kind: MemoryKind,
    content: String,
    reason: MemoryRetrievalReason,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryTraceItem {
    ordinal: usize,
    memory_id: MemoryId,
    kind: MemoryKind,
    reason: MemoryRetrievalReason,
    content_bytes: usize,
}

impl MemoryTraceItem {
    pub fn new(
        ordinal: usize,
        memory_id: MemoryId,
        kind: MemoryKind,
        reason: MemoryRetrievalReason,
        content_bytes: usize,
    ) -> Result<Self, RuntimeError> {
        if ordinal >= MAX_MEMORY_RETRIEVAL_ITEMS {
            return Err(memory_error("memory trace ordinal exceeds item limit"));
        }
        if !(1..=MAX_MEMORY_CONTENT_BYTES).contains(&content_bytes) {
            return Err(memory_error("memory trace content byte count is invalid"));
        }
        Ok(Self {
            ordinal,
            memory_id,
            kind,
            reason,
            content_bytes,
        })
    }

    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    pub const fn memory_id(&self) -> MemoryId {
        self.memory_id
    }

    pub const fn kind(&self) -> MemoryKind {
        self.kind
    }

    pub const fn reason(&self) -> MemoryRetrievalReason {
        self.reason
    }

    pub const fn content_bytes(&self) -> usize {
        self.content_bytes
    }
}

impl MemoryContextItem {
    pub fn new(
        memory_id: MemoryId,
        kind: MemoryKind,
        content: impl Into<String>,
        reason: MemoryRetrievalReason,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            memory_id,
            kind,
            content: validate_content(content.into())?,
            reason,
        })
    }

    pub const fn memory_id(&self) -> MemoryId {
        self.memory_id
    }

    pub const fn kind(&self) -> MemoryKind {
        self.kind
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    pub const fn content_bytes(&self) -> usize {
        self.content.len()
    }

    pub const fn reason(&self) -> MemoryRetrievalReason {
        self.reason
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryTraceExclusions {
    by_state: usize,
    by_expiry: usize,
    by_relevance: usize,
    by_item_limit: usize,
    by_byte_limit: usize,
}

impl MemoryTraceExclusions {
    pub const fn new(
        by_state: usize,
        by_expiry: usize,
        by_relevance: usize,
        by_item_limit: usize,
        by_byte_limit: usize,
    ) -> Self {
        Self {
            by_state,
            by_expiry,
            by_relevance,
            by_item_limit,
            by_byte_limit,
        }
    }

    pub const fn by_state(self) -> usize {
        self.by_state
    }

    pub const fn by_expiry(self) -> usize {
        self.by_expiry
    }

    pub const fn by_relevance(self) -> usize {
        self.by_relevance
    }

    pub const fn by_item_limit(self) -> usize {
        self.by_item_limit
    }

    pub const fn by_byte_limit(self) -> usize {
        self.by_byte_limit
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryRetrievalTrace {
    trace_id: RetrievalTraceId,
    turn_id: TurnId,
    created_at: UnixTimestampMillis,
    items: Vec<MemoryTraceItem>,
    exclusions: MemoryTraceExclusions,
}

impl MemoryRetrievalTrace {
    pub fn new(
        trace_id: RetrievalTraceId,
        turn_id: TurnId,
        created_at: UnixTimestampMillis,
        items: impl IntoIterator<Item = MemoryTraceItem>,
        exclusions: MemoryTraceExclusions,
    ) -> Result<Self, RuntimeError> {
        let items = items.into_iter().collect::<Vec<_>>();
        if items.len() > MAX_MEMORY_RETRIEVAL_ITEMS {
            return Err(memory_error("memory trace item count exceeds 8"));
        }
        for (ordinal, item) in items.iter().enumerate() {
            if item.ordinal() != ordinal {
                return Err(memory_error("memory trace item order is not contiguous"));
            }
            if items[..ordinal]
                .iter()
                .any(|prior| prior.memory_id() == item.memory_id())
            {
                return Err(memory_error("memory trace contains a duplicate memory"));
            }
        }
        let used_bytes = items.iter().try_fold(0_usize, |total, item| {
            total.checked_add(item.content_bytes())
        });
        let Some(used_bytes) = used_bytes else {
            return Err(memory_error("memory trace byte count overflowed"));
        };
        if used_bytes > MAX_MEMORY_RETRIEVAL_BYTES {
            return Err(memory_error("memory trace byte count exceeds 8192"));
        }
        Ok(Self {
            trace_id,
            turn_id,
            created_at,
            items,
            exclusions,
        })
    }

    pub const fn trace_id(&self) -> RetrievalTraceId {
        self.trace_id
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub const fn created_at(&self) -> UnixTimestampMillis {
        self.created_at
    }

    pub const fn selected_items(&self) -> usize {
        self.items.len()
    }

    pub fn used_bytes(&self) -> usize {
        self.items.iter().map(MemoryTraceItem::content_bytes).sum()
    }

    pub fn items(&self) -> &[MemoryTraceItem] {
        &self.items
    }

    pub const fn exclusions(&self) -> MemoryTraceExclusions {
        self.exclusions
    }

    pub fn metric_json(&self) -> String {
        let items = self
            .items
            .iter()
            .map(|item| {
                format!(
                    concat!(
                        "{{\"ordinal\":{},\"memory_id\":{},\"kind\":\"{}\",",
                        "\"reason\":\"{}\",\"content_bytes\":{}}}"
                    ),
                    item.ordinal(),
                    item.memory_id().get(),
                    item.kind().as_str(),
                    item.reason().as_str(),
                    item.content_bytes(),
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!(
            concat!(
                "{{\"trace_id\":{},\"turn_id\":{},\"created_at_ms\":{},",
                "\"selected_items\":{},\"used_bytes\":{},\"items\":[{}],",
                "\"excluded_by_state\":{},\"excluded_by_expiry\":{},",
                "\"excluded_by_relevance\":{},\"excluded_by_item_limit\":{},",
                "\"excluded_by_byte_limit\":{}}}"
            ),
            self.trace_id.get(),
            self.turn_id.get(),
            self.created_at.get(),
            self.selected_items(),
            self.used_bytes(),
            items,
            self.exclusions.by_state(),
            self.exclusions.by_expiry(),
            self.exclusions.by_relevance(),
            self.exclusions.by_item_limit(),
            self.exclusions.by_byte_limit(),
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryInspection {
    record: MemoryRecord,
    sources: Vec<MemoryProvenance>,
    approvals: Vec<MemoryApprovalEvidence>,
}

impl MemoryInspection {
    pub fn new(
        record: MemoryRecord,
        sources: impl IntoIterator<Item = MemoryProvenance>,
        approvals: impl IntoIterator<Item = MemoryApprovalEvidence>,
    ) -> Result<Self, RuntimeError> {
        let sources = sources.into_iter().collect::<Vec<_>>();
        let approvals = approvals.into_iter().collect::<Vec<_>>();
        if sources.is_empty() {
            return Err(memory_error("memory inspection requires provenance"));
        }
        if sources.last() != Some(record.provenance()) {
            return Err(memory_error(
                "memory inspection latest provenance does not match record",
            ));
        }
        if record
            .approval()
            .is_some_and(|approval| approvals.last() != Some(approval))
        {
            return Err(memory_error(
                "memory inspection current approval does not match approval history",
            ));
        }
        Ok(Self {
            record,
            sources,
            approvals,
        })
    }

    pub const fn record(&self) -> &MemoryRecord {
        &self.record
    }

    pub fn sources(&self) -> &[MemoryProvenance] {
        &self.sources
    }

    pub fn approvals(&self) -> &[MemoryApprovalEvidence] {
        &self.approvals
    }
}

fn content_digest(content: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(content.as_bytes()))
}

fn is_sha256_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_content(content: String) -> Result<String, RuntimeError> {
    bounded_text(
        content,
        MAX_MEMORY_CONTENT_BYTES,
        "memory content must not be empty",
        "memory content exceeds 4 KiB",
    )
}

fn bounded_text(
    value: String,
    maximum_bytes: usize,
    empty_message: &'static str,
    overflow_message: &'static str,
) -> Result<String, RuntimeError> {
    if value.trim().is_empty() {
        return Err(memory_error(empty_message));
    }
    if value.len() > maximum_bytes {
        return Err(memory_error(overflow_message));
    }
    Ok(value)
}

fn validate_retention(
    kind: MemoryKind,
    created_at: UnixTimestampMillis,
    retention: &MemoryRetention,
) -> Result<(), RuntimeError> {
    match (kind, retention) {
        (MemoryKind::Working, MemoryRetention::Working { expires_at }) => {
            let duration = expires_at
                .get()
                .checked_sub(created_at.get())
                .ok_or_else(|| memory_error("working memory expiry precedes creation"))?;
            if duration <= 0 {
                return Err(memory_error("working memory expiry must follow creation"));
            }
            if duration > MAX_WORKING_RETENTION_MILLIS {
                return Err(memory_error("working memory retention exceeds 24 hours"));
            }
        }
        (MemoryKind::Working, _) => {
            return Err(memory_error("working memory requires working retention"));
        }
        (_, MemoryRetention::Working { .. }) => {
            return Err(memory_error("working retention requires working memory"));
        }
        (_, MemoryRetention::Until { expires_at }) if *expires_at <= created_at => {
            return Err(memory_error("memory expiry must follow creation"));
        }
        _ => {}
    }
    Ok(())
}

fn validate_revision(revision: u64) -> Result<(), RuntimeError> {
    if revision == 0 {
        return Err(memory_error("memory revision must be non-zero"));
    }
    Ok(())
}

fn memory_error(message: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::Memory,
        message,
    )
}
