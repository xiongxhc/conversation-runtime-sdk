use conversation_protocol::{
    MemoryContextItem, MemoryRecord, MemoryRetrievalReason, MemoryRetrievalTrace,
    MemoryTraceExclusions,
};

use crate::{MemoryStoreError, MemoryStoreResult};

pub trait RetrievalCancellation: Send + Sync {
    fn is_cancelled(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct NeverCancelled;

impl RetrievalCancellation for NeverCancelled {
    fn is_cancelled(&self) -> bool {
        false
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MemoryRetrieval {
    items: Vec<MemoryContextItem>,
    trace: MemoryRetrievalTrace,
}

impl MemoryRetrieval {
    pub(crate) fn new(items: Vec<MemoryContextItem>, trace: MemoryRetrievalTrace) -> Self {
        Self { items, trace }
    }

    pub fn items(&self) -> &[MemoryContextItem] {
        &self.items
    }

    pub const fn trace(&self) -> &MemoryRetrievalTrace {
        &self.trace
    }
}

#[derive(Clone)]
pub(crate) struct RankedMemory {
    pub(crate) record: MemoryRecord,
    pub(crate) reason: MemoryRetrievalReason,
}

pub(crate) struct RankedSelection {
    pub(crate) selected: Vec<RankedMemory>,
    pub(crate) exclusions: MemoryTraceExclusions,
}

pub(crate) fn select_records(
    records: Vec<MemoryRecord>,
    query: &str,
    now: conversation_protocol::UnixTimestampMillis,
    maximum_items: usize,
    maximum_bytes: usize,
    cancellation: &dyn RetrievalCancellation,
) -> MemoryStoreResult<RankedSelection> {
    check_cancellation(cancellation)?;
    let normalized_query = normalize(query);
    let query_units = search_units(&normalized_query);
    let mut ranked = Vec::new();
    let mut excluded_by_state = 0;
    let mut excluded_by_expiry = 0;
    let mut excluded_by_relevance = 0;

    for record in records {
        check_cancellation(cancellation)?;
        if record.created_at() > now
            || record.updated_at() > now
            || record
                .last_used_at()
                .is_some_and(|last_used| last_used > now)
        {
            excluded_by_state += 1;
            continue;
        }
        if record.state() == conversation_protocol::MemoryState::Expired
            || record
                .retention()
                .expires_at()
                .is_some_and(|expires_at| now >= expires_at)
        {
            excluded_by_expiry += 1;
            continue;
        }
        if record.state() != conversation_protocol::MemoryState::Active {
            excluded_by_state += 1;
            continue;
        }
        let normalized_content = normalize(record.content());
        let exact = normalized_content.contains(&normalized_query)
            || normalized_query.contains(&normalized_content);
        let shared = if exact {
            false
        } else {
            let content_units = search_units(&normalized_content);
            query_units.iter().any(|unit| content_units.contains(unit))
        };
        let reason = if record.pinned() && (exact || shared) {
            Some(MemoryRetrievalReason::PinnedMatch)
        } else if exact {
            Some(MemoryRetrievalReason::ExactPhrase)
        } else if shared {
            Some(MemoryRetrievalReason::SharedTerm)
        } else if record.kind() == conversation_protocol::MemoryKind::Working {
            Some(MemoryRetrievalReason::RecentWorking)
        } else {
            None
        };
        let Some(reason) = reason else {
            excluded_by_relevance += 1;
            continue;
        };
        ranked.push(RankedMemory { record, reason });
    }

    ranked.sort_by(|left, right| {
        reason_priority(right.reason)
            .cmp(&reason_priority(left.reason))
            .then_with(|| {
                right
                    .record
                    .confidence()
                    .get()
                    .cmp(&left.record.confidence().get())
            })
            .then_with(|| {
                right
                    .record
                    .last_used_at()
                    .unwrap_or(right.record.created_at())
                    .cmp(
                        &left
                            .record
                            .last_used_at()
                            .unwrap_or(left.record.created_at()),
                    )
            })
            .then_with(|| left.record.id().cmp(&right.record.id()))
    });

    let mut selected = Vec::new();
    let mut used_bytes = 0_usize;
    let mut excluded_by_item_limit = 0;
    let mut excluded_by_byte_limit = 0;
    for item in ranked {
        check_cancellation(cancellation)?;
        if selected.len() == maximum_items {
            excluded_by_item_limit += 1;
            continue;
        }
        let Some(next_bytes) = used_bytes.checked_add(item.record.content().len()) else {
            excluded_by_byte_limit += 1;
            continue;
        };
        if next_bytes > maximum_bytes {
            excluded_by_byte_limit += 1;
            continue;
        }
        used_bytes = next_bytes;
        selected.push(item);
    }

    Ok(RankedSelection {
        selected,
        exclusions: MemoryTraceExclusions::new(
            excluded_by_state,
            excluded_by_expiry,
            excluded_by_relevance,
            excluded_by_item_limit,
            excluded_by_byte_limit,
        ),
    })
}

pub(crate) fn check_cancellation(
    cancellation: &dyn RetrievalCancellation,
) -> MemoryStoreResult<()> {
    if cancellation.is_cancelled() {
        return Err(MemoryStoreError::cancelled());
    }
    Ok(())
}

fn reason_priority(reason: MemoryRetrievalReason) -> u8 {
    match reason {
        MemoryRetrievalReason::PinnedMatch => 4,
        MemoryRetrievalReason::ExactPhrase => 3,
        MemoryRetrievalReason::SharedTerm => 2,
        MemoryRetrievalReason::RecentWorking => 1,
        _ => 0,
    }
}

fn normalize(value: &str) -> String {
    value.trim().to_lowercase()
}

fn search_units(value: &str) -> Vec<String> {
    let mut units = Vec::new();
    let mut run = String::new();
    for character in value.chars().chain([' ']) {
        if character.is_alphanumeric() {
            run.push(character);
            continue;
        }
        if run.is_empty() {
            continue;
        }
        if run.is_ascii() {
            units.push(std::mem::take(&mut run));
            continue;
        }
        let characters = run.chars().collect::<Vec<_>>();
        if characters.len() == 1 {
            units.push(characters[0].to_string());
        } else {
            units.extend(
                characters
                    .windows(2)
                    .map(|pair| pair.iter().collect::<String>()),
            );
        }
        run.clear();
    }
    units.sort();
    units.dedup();
    units
}
