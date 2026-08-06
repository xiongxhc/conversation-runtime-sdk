use crate::{
    MemoryApprovalEvidence, MemoryId, MemoryInspection, MemoryProvenance, MemoryRecord,
    MemoryRetention,
};

pub const MAX_MEMORY_LIST_PAGE_ITEMS: usize = 50;
pub const MAX_MEMORY_PREVIEW_BYTES: usize = 192;
pub const MAX_MEMORY_INSPECTION_HISTORY_ITEMS: usize = 32;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemorySummary {
    pub id: String,
    pub content_preview: String,
    pub kind: String,
    pub state: String,
    pub pinned: bool,
    pub updated_at_ms: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemoryPage {
    pub records: Vec<ClientMemorySummary>,
    pub next_cursor: Option<ClientMemoryCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemoryCursor {
    pub before_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemoryRecord {
    pub id: String,
    pub kind: String,
    pub content: String,
    pub state: String,
    pub confidence: String,
    pub created_at_ms: String,
    pub updated_at_ms: String,
    pub pinned: bool,
    pub revision: String,
    pub retention: ClientMemoryRetention,
    pub last_used_at_ms: Option<String>,
    pub last_retrieval_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemoryInspection {
    pub record: ClientMemoryRecord,
    pub sources: Vec<ClientMemoryProvenance>,
    pub approvals: Vec<ClientMemoryApproval>,
    pub sources_truncated: bool,
    pub approvals_truncated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemoryProvenance {
    pub kind: String,
    pub source_id: String,
    pub source_timestamp_ms: String,
    pub actor: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClientMemoryApproval {
    pub confirmation_id: String,
    pub actor: String,
    pub confirmed_at_ms: String,
    pub approved_revision: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClientMemoryRetention {
    Working { expires_at_ms: String },
    Session { session_id: String },
    Until { expires_at_ms: String },
    UntilDeleted,
}

impl From<&MemoryRecord> for ClientMemorySummary {
    fn from(record: &MemoryRecord) -> Self {
        Self {
            id: record.id().get().to_string(),
            content_preview: memory_preview(record.content()),
            kind: record.kind().as_str().to_owned(),
            state: record.state().as_str().to_owned(),
            pinned: record.pinned(),
            updated_at_ms: record.updated_at().get().to_string(),
        }
    }
}

impl From<MemoryId> for ClientMemoryCursor {
    fn from(before_id: MemoryId) -> Self {
        Self {
            before_id: before_id.get().to_string(),
        }
    }
}

impl From<&MemoryRecord> for ClientMemoryRecord {
    fn from(record: &MemoryRecord) -> Self {
        Self {
            id: record.id().get().to_string(),
            kind: record.kind().as_str().to_owned(),
            content: record.content().to_owned(),
            state: record.state().as_str().to_owned(),
            confidence: record.confidence().get().to_string(),
            created_at_ms: record.created_at().get().to_string(),
            updated_at_ms: record.updated_at().get().to_string(),
            pinned: record.pinned(),
            revision: record.revision().to_string(),
            retention: ClientMemoryRetention::from(record.retention()),
            last_used_at_ms: record.last_used_at().map(|value| value.get().to_string()),
            last_retrieval_reason: record
                .last_retrieval_reason()
                .map(|reason| reason.as_str().to_owned()),
        }
    }
}

impl From<&MemoryInspection> for ClientMemoryInspection {
    fn from(inspection: &MemoryInspection) -> Self {
        let sources = inspection.sources();
        let approvals = inspection.approvals();
        let source_start = sources
            .len()
            .saturating_sub(MAX_MEMORY_INSPECTION_HISTORY_ITEMS);
        let approval_start = approvals
            .len()
            .saturating_sub(MAX_MEMORY_INSPECTION_HISTORY_ITEMS);

        Self {
            record: ClientMemoryRecord::from(inspection.record()),
            sources: sources[source_start..]
                .iter()
                .map(ClientMemoryProvenance::from)
                .collect(),
            approvals: approvals[approval_start..]
                .iter()
                .map(ClientMemoryApproval::from)
                .collect(),
            sources_truncated: source_start > 0,
            approvals_truncated: approval_start > 0,
        }
    }
}

impl From<&MemoryProvenance> for ClientMemoryProvenance {
    fn from(provenance: &MemoryProvenance) -> Self {
        Self {
            kind: provenance.kind().as_str().to_owned(),
            source_id: provenance.source_id().to_owned(),
            source_timestamp_ms: provenance.source_timestamp().get().to_string(),
            actor: provenance.actor().to_owned(),
        }
    }
}

impl From<&MemoryApprovalEvidence> for ClientMemoryApproval {
    fn from(approval: &MemoryApprovalEvidence) -> Self {
        Self {
            confirmation_id: approval.confirmation_id().to_owned(),
            actor: approval.actor().to_owned(),
            confirmed_at_ms: approval.confirmed_at().get().to_string(),
            approved_revision: approval.approved_revision().to_string(),
        }
    }
}

impl From<&MemoryRetention> for ClientMemoryRetention {
    fn from(retention: &MemoryRetention) -> Self {
        match retention {
            MemoryRetention::Working { expires_at } => Self::Working {
                expires_at_ms: expires_at.get().to_string(),
            },
            MemoryRetention::Session { session_id } => Self::Session {
                session_id: session_id.get().to_string(),
            },
            MemoryRetention::Until { expires_at } => Self::Until {
                expires_at_ms: expires_at.get().to_string(),
            },
            MemoryRetention::UntilDeleted => Self::UntilDeleted,
        }
    }
}

pub fn memory_preview(content: &str) -> String {
    let normalized = content.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8_with_ellipsis(&normalized, MAX_MEMORY_PREVIEW_BYTES)
}

fn truncate_utf8_with_ellipsis(content: &str, maximum_bytes: usize) -> String {
    if content.len() <= maximum_bytes {
        return content.to_owned();
    }

    let ellipsis = "…";
    let mut end = maximum_bytes.saturating_sub(ellipsis.len());
    while !content.is_char_boundary(end) {
        end -= 1;
    }

    let mut truncated = String::with_capacity(maximum_bytes);
    truncated.push_str(&content[..end]);
    truncated.push_str(ellipsis);
    truncated
}
