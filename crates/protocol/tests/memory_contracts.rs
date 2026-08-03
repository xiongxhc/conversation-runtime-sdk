use conversation_protocol::{
    MemoryApproval, MemoryConfidence, MemoryContextItem, MemoryDraft, MemoryId, MemoryInspection,
    MemoryKind, MemoryPatch, MemoryProvenance, MemoryProvenanceKind, MemoryRecord, MemoryRetention,
    MemoryRetrievalReason, MemoryRetrievalRequest, MemoryRetrievalTrace, MemoryState,
    MemoryTraceExclusions, MemoryTraceItem, RetrievalTraceId, RuntimeErrorKind, RuntimeEvent,
    RuntimeStage, TurnId, UnixTimestampMillis, MAX_MEMORY_CONTENT_BYTES,
    MAX_MEMORY_RETRIEVAL_BYTES, MAX_MEMORY_RETRIEVAL_ITEMS,
};

fn timestamp(value: i64) -> UnixTimestampMillis {
    UnixTimestampMillis::new(value).unwrap()
}

fn provenance(kind: MemoryProvenanceKind, reference: &str) -> MemoryProvenance {
    MemoryProvenance::new(
        kind,
        reference,
        timestamp(900),
        "local-user",
        Some("sha256:source-digest".to_owned()),
    )
    .unwrap()
}

#[test]
fn identifiers_timestamps_confidence_and_enums_are_bounded_and_stable() {
    assert!(MemoryId::new(0).is_err());
    assert_eq!(MemoryId::new(7).unwrap().get(), 7);
    assert!(RetrievalTraceId::new(0).is_err());
    assert_eq!(RetrievalTraceId::new(9).unwrap().get(), 9);
    assert!(UnixTimestampMillis::new(-1).is_err());
    assert_eq!(timestamp(1_000).get(), 1_000);
    assert_eq!(MemoryConfidence::new(1_000).unwrap().get(), 1_000);
    assert!(MemoryConfidence::new(1_001).is_err());

    assert_eq!(MemoryKind::Working.as_str(), "working");
    assert_eq!(MemoryKind::Episodic.as_str(), "episodic");
    assert_eq!(MemoryKind::Semantic.as_str(), "semantic");
    assert_eq!(MemoryKind::Identity.as_str(), "identity");
    assert_eq!(MemoryKind::Relationship.as_str(), "relationship");
    assert_eq!(MemoryState::Candidate.as_str(), "candidate");
    assert_eq!(MemoryState::Active.as_str(), "active");
    assert_eq!(MemoryState::Expired.as_str(), "expired");
    assert_eq!(RuntimeStage::Memory.as_str(), "memory");
}

#[test]
fn drafts_require_visible_provenance_valid_content_and_bounded_retention() {
    let created_at = timestamp(1_000);
    let working = MemoryDraft::new(
        MemoryKind::Working,
        "Call the dentist tomorrow",
        provenance(MemoryProvenanceKind::UserProvided, "voice-turn:7"),
        MemoryConfidence::new(900).unwrap(),
        created_at,
        MemoryRetention::working(timestamp(2_000)),
    )
    .unwrap();
    assert_eq!(working.initial_state(), MemoryState::Active);
    assert_eq!(working.content(), "Call the dentist tomorrow");
    assert_eq!(working.retention().expires_at(), Some(timestamp(2_000)));

    let identity = MemoryDraft::new(
        MemoryKind::Identity,
        "The user prefers concise answers",
        provenance(MemoryProvenanceKind::CompletedExchange, "turn:8"),
        MemoryConfidence::new(700).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    assert_eq!(identity.initial_state(), MemoryState::Candidate);

    let relationship = MemoryDraft::new(
        MemoryKind::Relationship,
        "Shared humor is welcome",
        provenance(MemoryProvenanceKind::UserProvided, "settings"),
        MemoryConfidence::new(800).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    assert_eq!(relationship.initial_state(), MemoryState::Candidate);

    assert!(MemoryDraft::new(
        MemoryKind::Semantic,
        " ",
        provenance(MemoryProvenanceKind::UserProvided, "settings"),
        MemoryConfidence::new(800).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .is_err());
    assert!(MemoryDraft::new(
        MemoryKind::Semantic,
        "x".repeat(MAX_MEMORY_CONTENT_BYTES + 1),
        provenance(MemoryProvenanceKind::UserProvided, "settings"),
        MemoryConfidence::new(800).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .is_err());
    assert!(MemoryDraft::new(
        MemoryKind::Working,
        "invalid retention",
        provenance(MemoryProvenanceKind::UserProvided, "settings"),
        MemoryConfidence::new(800).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .is_err());
    assert!(MemoryDraft::new(
        MemoryKind::Working,
        "too long",
        provenance(MemoryProvenanceKind::UserProvided, "settings"),
        MemoryConfidence::new(800).unwrap(),
        created_at,
        MemoryRetention::working(timestamp(1_000 + 24 * 60 * 60 * 1_000 + 1)),
    )
    .is_err());
}

#[test]
fn records_and_patches_keep_inspection_metadata_typed() {
    let created_at = timestamp(1_000);
    let draft = MemoryDraft::new(
        MemoryKind::Semantic,
        "The project uses explicit local providers",
        provenance(MemoryProvenanceKind::ApplicationImported, "settings:v2"),
        MemoryConfidence::new(750).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    let record = MemoryRecord::new(
        MemoryId::new(4).unwrap(),
        draft,
        MemoryState::Active,
        timestamp(1_500),
        false,
        2,
        None,
        Some(timestamp(1_400)),
        Some(MemoryRetrievalReason::SharedTerm),
    )
    .unwrap();
    assert_eq!(record.id(), MemoryId::new(4).unwrap());
    assert_eq!(record.kind(), MemoryKind::Semantic);
    assert_eq!(record.revision(), 2);
    assert_eq!(record.last_used_at(), Some(timestamp(1_400)));
    assert_eq!(
        record.last_retrieval_reason(),
        Some(MemoryRetrievalReason::SharedTerm)
    );

    let patch = MemoryPatch::new(
        2,
        Some("The project requires explicit local providers".to_owned()),
        Some(MemoryConfidence::new(900).unwrap()),
        None,
        timestamp(2_000),
        provenance(MemoryProvenanceKind::UserEdited, "memory-probe"),
    )
    .unwrap();
    assert_eq!(
        patch.content(),
        Some("The project requires explicit local providers")
    );
    assert_eq!(patch.edited_at(), timestamp(2_000));
    assert_eq!(patch.expected_revision(), 2);
    assert!(MemoryPatch::new(
        2,
        None,
        None,
        None,
        timestamp(2_000),
        provenance(MemoryProvenanceKind::UserEdited, "memory-probe"),
    )
    .is_err());

    let approval =
        MemoryApproval::new("confirmation:42", "local-user", timestamp(2_100), 2).unwrap();
    assert_eq!(approval.confirmation_id(), "confirmation:42");
    assert_eq!(approval.actor(), "local-user");
    assert_eq!(approval.expected_revision(), 2);

    let approval_evidence = approval.evidence_for("The user prefers concise answers");
    let approved_draft = MemoryDraft::new(
        MemoryKind::Identity,
        "The user prefers concise answers",
        provenance(MemoryProvenanceKind::CompletedExchange, "turn:8"),
        MemoryConfidence::new(900).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    let approved = MemoryRecord::new(
        MemoryId::new(5).unwrap(),
        approved_draft,
        MemoryState::Active,
        timestamp(2_100),
        false,
        3,
        Some(approval_evidence.clone()),
        None,
        None,
    )
    .unwrap();
    let inspection = MemoryInspection::new(
        approved,
        [provenance(
            MemoryProvenanceKind::CompletedExchange,
            "turn:8",
        )],
        [approval_evidence.clone()],
    )
    .unwrap();
    assert_eq!(inspection.sources().len(), 1);
    assert_eq!(inspection.approvals(), &[approval_evidence]);

    let mismatched_evidence = approval.evidence_for("different content");
    let mismatched_draft = MemoryDraft::new(
        MemoryKind::Identity,
        "The user prefers concise answers",
        provenance(MemoryProvenanceKind::CompletedExchange, "turn:8"),
        MemoryConfidence::new(900).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    assert!(MemoryRecord::new(
        MemoryId::new(7).unwrap(),
        mismatched_draft,
        MemoryState::Active,
        timestamp(2_100),
        false,
        3,
        Some(mismatched_evidence),
        None,
        None,
    )
    .is_err());

    let unapproved_identity = MemoryDraft::new(
        MemoryKind::Identity,
        "Unapproved identity",
        provenance(MemoryProvenanceKind::CompletedExchange, "turn:9"),
        MemoryConfidence::new(500).unwrap(),
        created_at,
        MemoryRetention::UntilDeleted,
    )
    .unwrap();
    assert!(MemoryRecord::new(
        MemoryId::new(6).unwrap(),
        unapproved_identity,
        MemoryState::Active,
        timestamp(2_200),
        false,
        2,
        None,
        None,
        None,
    )
    .is_err());
}

#[test]
fn retrieval_requests_and_context_items_enforce_declared_budgets() {
    let request = MemoryRetrievalRequest::new(
        TurnId::new(12),
        "dentist appointment",
        timestamp(5_000),
        MAX_MEMORY_RETRIEVAL_ITEMS,
        MAX_MEMORY_RETRIEVAL_BYTES,
    )
    .unwrap();
    assert_eq!(request.turn_id(), TurnId::new(12));
    assert_eq!(request.query(), "dentist appointment");
    assert_eq!(request.maximum_items(), MAX_MEMORY_RETRIEVAL_ITEMS);
    assert_eq!(request.maximum_bytes(), MAX_MEMORY_RETRIEVAL_BYTES);

    assert!(MemoryRetrievalRequest::new(TurnId::new(1), "", timestamp(0), 1, 1).is_err());
    assert!(MemoryRetrievalRequest::new(
        TurnId::new(1),
        "query",
        timestamp(0),
        MAX_MEMORY_RETRIEVAL_ITEMS + 1,
        1,
    )
    .is_err());
    assert!(MemoryRetrievalRequest::new(
        TurnId::new(1),
        "query",
        timestamp(0),
        1,
        MAX_MEMORY_RETRIEVAL_BYTES + 1,
    )
    .is_err());

    let item = MemoryContextItem::new(
        MemoryId::new(3).unwrap(),
        MemoryKind::Working,
        "Dentist appointment is tomorrow",
        MemoryRetrievalReason::ExactPhrase,
    )
    .unwrap();
    assert_eq!(item.memory_id(), MemoryId::new(3).unwrap());
    assert_eq!(item.reason(), MemoryRetrievalReason::ExactPhrase);
    assert_eq!(item.content_bytes(), 31);
}

#[test]
fn retrieval_trace_metrics_are_content_free_and_strictly_bounded() {
    let items = [
        MemoryTraceItem::new(
            0,
            MemoryId::new(7).unwrap(),
            MemoryKind::Semantic,
            MemoryRetrievalReason::ExactPhrase,
            40,
        )
        .unwrap(),
        MemoryTraceItem::new(
            1,
            MemoryId::new(8).unwrap(),
            MemoryKind::Episodic,
            MemoryRetrievalReason::SharedTerm,
            51,
        )
        .unwrap(),
    ];
    let trace = MemoryRetrievalTrace::new(
        RetrievalTraceId::new(4).unwrap(),
        TurnId::new(12),
        timestamp(5_000),
        items,
        MemoryTraceExclusions::new(1, 2, 3, 4, 5),
    )
    .unwrap();

    let json = trace.metric_json();
    assert_eq!(trace.selected_items(), 2);
    assert_eq!(trace.used_bytes(), 91);
    assert!(json.contains("\"trace_id\":4"));
    assert!(json.contains("\"turn_id\":12"));
    assert!(json.contains("\"excluded_by_expiry\":2"));
    assert!(json.contains("\"memory_id\":7"));
    assert!(json.contains("\"reason\":\"exact_phrase\""));
    for forbidden in ["dentist", "query", "\"content\":", "transcript"] {
        assert!(!json.contains(forbidden));
    }

    let event = RuntimeEvent::MemoryRetrieved {
        trace: trace.clone(),
    };
    assert_eq!(event.turn_id(), TurnId::new(12));
    assert!(!event.is_terminal());
    let event_json = event.memory_metric_json().unwrap();
    assert!(event_json.contains("\"event\":\"memory_retrieved\""));
    assert!(event_json.contains("\"trace_id\":4"));
    assert!(event_json.contains("\"selected_items\":2"));
    assert!(event_json.contains("\"used_bytes\":91"));
    assert!(!event_json.contains("created_at"));
    assert!(!event_json.contains("excluded"));
    assert!(!event_json.contains("memory_id"));

    let error = MemoryRetrievalTrace::new(
        RetrievalTraceId::new(5).unwrap(),
        TurnId::new(12),
        timestamp(5_000),
        (0..=MAX_MEMORY_RETRIEVAL_ITEMS)
            .map(|ordinal| {
                MemoryTraceItem::new(
                    ordinal % MAX_MEMORY_RETRIEVAL_ITEMS,
                    MemoryId::new(ordinal as u64 + 1).unwrap(),
                    MemoryKind::Semantic,
                    MemoryRetrievalReason::SharedTerm,
                    1,
                )
                .unwrap()
            })
            .collect::<Vec<_>>(),
        MemoryTraceExclusions::default(),
    )
    .unwrap_err();
    assert_eq!(error.kind(), RuntimeErrorKind::Configuration);
    assert_eq!(error.stage(), RuntimeStage::Memory);

    let duplicate = MemoryTraceItem::new(
        1,
        MemoryId::new(7).unwrap(),
        MemoryKind::Semantic,
        MemoryRetrievalReason::SharedTerm,
        1,
    )
    .unwrap();
    assert!(MemoryRetrievalTrace::new(
        RetrievalTraceId::new(6).unwrap(),
        TurnId::new(12),
        timestamp(5_000),
        [trace.items()[0].clone(), duplicate],
        MemoryTraceExclusions::default(),
    )
    .is_err());
}
