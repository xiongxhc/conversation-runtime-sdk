use conversation_desktop::history_store::{
    ContinuationState, ConversationContextExchange, ConversationHistory, ConversationHistoryStore,
    ConversationHistoryTurn, ConversationOrigin, HistoryRevision, HistoryStoreErrorKind, TurnState,
};
use rusqlite::{params, Connection};

const REVISION_CONFLICT: &str = "conversation history revision conflict";
const NOT_FOUND: &str = "conversation history was not found";
const NO_ELIGIBLE_CONTEXT: &str = "This Session has no completed exchanges to continue.";
const LATEST_EXCHANGE_TOO_LARGE: &str =
    "The latest exchange is too large to continue without shortening or compression.";
const INVALID_CONTINUATION_WRITE: &str = "conversation history continuation data is invalid";

#[test]
fn migrates_legacy_schema_zero_without_changing_saved_content() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("history.sqlite3");
    create_legacy_schema_zero(&database);

    let store = ConversationHistoryStore::open(&database).unwrap();
    let migrated = store.get("legacy").unwrap().unwrap();

    assert_eq!(user_version(&database), 2);
    assert_eq!(migrated.revision, revision(1));
    assert_eq!(migrated.continued_from_id, None);
    assert_eq!(migrated.continuation_operation_id, None);
    assert_eq!(migrated.continuation_state, None);
    assert_eq!(migrated.title, "Legacy title");
    assert_eq!(migrated.created_at_ms, 10);
    assert_eq!(migrated.updated_at_ms, 20);
    assert_eq!(migrated.turns.len(), 1);
    assert_eq!(migrated.turns[0].turn_id, "legacy-turn");
    assert_eq!(migrated.turns[0].transcript, "Legacy question\nline two");
    assert_eq!(migrated.turns[0].response, "Legacy answer\nline two");
    assert_eq!(migrated.turns[0].state, TurnState::Completed);
    assert_eq!(migrated.turns[0].failure_message, None);
    assert_eq!(migrated.turns[0].origin, ConversationOrigin::Live);
}

#[test]
fn creates_fresh_databases_at_schema_two() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("nested/history.sqlite3");

    let store = ConversationHistoryStore::open(&database).unwrap();
    let saved = store
        .save_revisioned(&conversation("fresh", 10, "Fresh schema"), None)
        .unwrap();

    assert_eq!(user_version(&database), 2);
    assert_eq!(saved, revision(1));
    assert_eq!(store.get("fresh").unwrap().unwrap().revision, revision(1));
}

#[test]
fn revisioned_insert_update_and_stale_update_are_compare_and_write() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let original = conversation("same", 10, "Initial title");

    let inserted = store.save_revisioned(&original, None).unwrap();
    assert_eq!(inserted, revision(1));

    let mut updated = original.clone();
    updated.title = "Updated title".to_owned();
    updated.updated_at_ms = 30;
    updated.turns = vec![turn("2", "Replacement question", "Replacement answer")];
    let updated_revision = store.save_revisioned(&updated, Some(inserted)).unwrap();
    assert_eq!(updated_revision, revision(2));

    let mut stale = original;
    stale.title = "Stale title".to_owned();
    stale.updated_at_ms = 40;
    stale.turns = vec![turn("3", "Stale question", "Stale answer")];
    assert_error(
        store.save_revisioned(&stale, Some(inserted)),
        REVISION_CONFLICT,
    );

    let canonical = store.get("same").unwrap().unwrap();
    assert_eq!(canonical.revision, revision(2));
    assert_eq!(canonical.title, "Updated title");
    assert_eq!(canonical.turns, updated.turns);
}

#[test]
fn new_insert_does_not_overwrite_an_existing_id() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let original = conversation("same", 10, "Initial title");
    store.save_revisioned(&original, None).unwrap();
    let mut duplicate = original.clone();
    duplicate.title = "Duplicate title".to_owned();
    duplicate.updated_at_ms = 20;

    assert_error(store.save_revisioned(&duplicate, None), REVISION_CONFLICT);
    assert_eq!(store.get("same").unwrap().unwrap().title, "Initial title");
}

#[test]
fn revisioned_delete_distinguishes_conflict_from_missing_id() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let inserted = store
        .save_revisioned(&conversation("delete-me", 10, "Delete me"), None)
        .unwrap();

    assert_error(
        store.delete_revisioned("delete-me", revision(2)),
        REVISION_CONFLICT,
    );
    assert!(store.get("delete-me").unwrap().is_some());
    assert_error(store.delete_revisioned("missing", revision(1)), NOT_FOUND);

    store.delete_revisioned("delete-me", inserted).unwrap();
    assert!(store.get("delete-me").unwrap().is_none());
}

#[test]
fn stale_save_after_delete_cannot_recreate_the_record() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let value = conversation("deleted", 10, "Deleted conversation");
    let inserted = store.save_revisioned(&value, None).unwrap();
    store.delete_revisioned("deleted", inserted).unwrap();

    assert_error(
        store.save_revisioned(&value, Some(inserted)),
        REVISION_CONFLICT,
    );
    assert!(store.get("deleted").unwrap().is_none());
}

#[test]
fn delete_cascades_turns_after_close_and_reopen() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("history.sqlite3");
    let store = ConversationHistoryStore::open(&database).unwrap();
    let inserted = store
        .save_revisioned(&conversation("cascade", 10, "Cascade"), None)
        .unwrap();
    store.delete_revisioned("cascade", inserted).unwrap();
    drop(store);

    let reopened = ConversationHistoryStore::open(&database).unwrap();
    assert!(reopened.get("cascade").unwrap().is_none());
    drop(reopened);
    let connection = Connection::open(&database).unwrap();
    let turn_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM conversation_turns WHERE conversation_id = 'cascade'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(turn_count, 0);
}

#[test]
fn lists_every_saved_conversation_with_canonical_revisions() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    for index in 0..201 {
        store
            .save_revisioned(
                &conversation(
                    &format!("conversation-{index}"),
                    index + 1,
                    &format!("Conversation {index}"),
                ),
                None,
            )
            .unwrap();
    }

    let summaries = store.list().unwrap();

    assert_eq!(summaries.len(), 201);
    assert_eq!(summaries[0].id, "conversation-200");
    assert_eq!(summaries[0].revision, revision(1));
    assert_eq!(summaries[200].id, "conversation-0");
}

#[test]
fn rejects_invalid_history_without_mutating_existing_data() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let original = conversation("valid", 10, "Valid title");
    let inserted = store.save_revisioned(&original, None).unwrap();
    let mut invalid = original.clone();
    invalid.title = " ".to_owned();

    assert!(store.save_revisioned(&invalid, Some(inserted)).is_err());
    assert_eq!(store.get("valid").unwrap().unwrap().title, original.title);
    assert_eq!(store.get("valid").unwrap().unwrap().revision, inserted);
}

#[test]
fn accepts_and_emits_the_frontend_camel_case_shape() {
    let value = serde_json::json!({
        "id": "shape",
        "title": "Shape test",
        "createdAtMs": 1,
        "updatedAtMs": 2,
        "turns": [{
            "turnId": "1",
            "transcript": "Hello",
            "response": "Hi",
            "state": "completed"
        }]
    });

    let conversation: ConversationHistory = serde_json::from_value(value).unwrap();
    let encoded = serde_json::to_value(&conversation).unwrap();

    assert_eq!(encoded["revision"], "1");
    assert_eq!(encoded["createdAtMs"], 1);
    assert_eq!(encoded["continuedFromId"], serde_json::Value::Null);
    assert_eq!(encoded["continuationOperationId"], serde_json::Value::Null);
    assert_eq!(encoded["continuationState"], serde_json::Value::Null);
    assert_eq!(encoded["turns"][0]["turnId"], "1");
    assert_eq!(encoded["turns"][0]["origin"], "live");
    assert_eq!(
        encoded["turns"][0]["failureMessage"],
        serde_json::Value::Null
    );
}

#[test]
fn history_revisions_are_positive_sqlite_integers() {
    assert!(HistoryRevision::new(0).is_err());
    assert_eq!(HistoryRevision::new(1).unwrap().get(), 1);
    assert!(HistoryRevision::new(i64::MAX as u64 + 1).is_err());
}

#[test]
fn history_revision_json_requires_canonical_positive_decimal_strings() {
    for invalid in [
        r#""0""#,
        r#""01""#,
        r#""+1""#,
        r#""-1""#,
        r#""9223372036854775808""#,
        "1",
    ] {
        assert!(
            serde_json::from_str::<HistoryRevision>(invalid).is_err(),
            "accepted invalid revision {invalid}"
        );
    }

    let maximum: HistoryRevision = serde_json::from_str(r#""9223372036854775807""#).unwrap();
    assert_eq!(maximum.get(), i64::MAX as u64);
    assert_eq!(
        serde_json::to_string(&maximum).unwrap(),
        r#""9223372036854775807""#
    );
}

#[test]
fn shares_one_store_across_threads() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ConversationHistoryStore>();

    let temporary = tempfile::tempdir().unwrap();
    let store = std::sync::Arc::new(
        ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap(),
    );
    let workers: Vec<_> = (0..4)
        .map(|index| {
            let store = std::sync::Arc::clone(&store);
            std::thread::spawn(move || {
                store
                    .save_revisioned(
                        &conversation(
                            &format!("thread-{index}"),
                            index + 1,
                            &format!("Thread {index}"),
                        ),
                        None,
                    )
                    .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    assert_eq!(store.list().unwrap().len(), 4);
}

#[test]
fn prepares_only_completed_nonblank_pairs_and_preserves_original_content() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("history.sqlite3");
    let store = ConversationHistoryStore::open(&database).unwrap();
    let mut source = conversation("source", 10, "Source title");
    source.turns = vec![
        turn("eligible-old", "  first user  ", "\nfirst assistant\n"),
        turn_with_state(
            "streaming",
            "streaming user",
            "streaming assistant",
            TurnState::Streaming,
        ),
        turn("blank-user", "temporarily nonblank", "assistant"),
        turn_with_state(
            "cancelled",
            "cancelled user",
            "cancelled assistant",
            TurnState::Cancelled,
        ),
        turn_with_state(
            "failed",
            "failed user",
            "failed assistant",
            TurnState::Failed,
        ),
        ConversationHistoryTurn {
            turn_id: "eligible-new".to_owned(),
            transcript: "new user".to_owned(),
            response: "new assistant".to_owned(),
            state: TurnState::Completed,
            failure_message: Some("metadata is not context".to_owned()),
            origin: ConversationOrigin::Live,
        },
    ];
    let source_revision = store.save_revisioned(&source, None).unwrap();
    Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE conversation_turns SET transcript = ' \n '
             WHERE conversation_id = 'source' AND turn_id = 'blank-user'",
            [],
        )
        .unwrap();
    source.turns[2].transcript = " \n ".to_owned();

    let prepared = store
        .prepare_continuation("source", source_revision, 25, "branch", "operation-1")
        .unwrap();

    assert_eq!(
        prepared.seed,
        vec![
            seed("  first user  ", "\nfirst assistant\n"),
            seed("new user", "new assistant"),
        ]
    );
    assert_eq!(prepared.operation_id, "operation-1");
    assert_eq!(prepared.branch.id, "branch");
    assert_eq!(prepared.branch.title, "Continued: Source title");
    assert_eq!(prepared.branch.created_at_ms, 25);
    assert_eq!(prepared.branch.updated_at_ms, 25);
    assert_eq!(prepared.branch.revision, revision(1));
    assert_eq!(prepared.branch.continued_from_id.as_deref(), Some("source"));
    assert_eq!(
        prepared.branch.continuation_operation_id.as_deref(),
        Some("operation-1")
    );
    assert_eq!(
        prepared.branch.continuation_state,
        Some(ContinuationState::Preparing)
    );
    assert_eq!(prepared.branch.turns.len(), 2);
    assert_eq!(prepared.branch.turns[0].transcript, "  first user  ");
    assert_eq!(prepared.branch.turns[0].response, "\nfirst assistant\n");
    assert_eq!(prepared.branch.turns[1].transcript, "new user");
    assert_eq!(prepared.branch.turns[1].response, "new assistant");
    assert!(prepared
        .branch
        .turns
        .iter()
        .all(|turn| turn.origin == ConversationOrigin::ContinuedContext
            && turn.state == TurnState::Completed
            && turn.failure_message.is_none()));
    assert_eq!(
        store.get("source").unwrap().unwrap(),
        source_with_revision(source, source_revision)
    );
    assert_eq!(store.get("branch").unwrap().unwrap(), prepared.branch);
}

#[test]
fn retains_the_newest_sixteen_pairs_and_returns_them_oldest_first() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let mut source = conversation("source", 10, "Source");
    source.turns = (0..18)
        .map(|index| {
            turn(
                &format!("turn-{index}"),
                &format!("user-{index}"),
                &format!("assistant-{index}"),
            )
        })
        .collect();
    let source_revision = store.save_revisioned(&source, None).unwrap();

    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    assert_eq!(prepared.seed.len(), 16);
    assert_eq!(
        prepared.seed.first().unwrap(),
        &seed("user-2", "assistant-2")
    );
    assert_eq!(
        prepared.seed.last().unwrap(),
        &seed("user-17", "assistant-17")
    );
}

#[test]
fn accepts_exact_utf8_message_and_aggregate_bounds() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let user = "🦀".repeat(4_096);
    let assistant = "界".repeat(5_461) + "a";
    assert_eq!(user.len(), 16_384);
    assert_eq!(assistant.len(), 16_384);
    let mut source = conversation("source", 10, "Source");
    source.turns = vec![turn("exact", &user, &assistant)];
    let source_revision = store.save_revisioned(&source, None).unwrap();

    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    assert_eq!(prepared.seed, vec![seed(&user, &assistant)]);
}

#[test]
fn rejects_an_oversized_newest_pair_with_a_typed_error() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let mut source = conversation("source", 10, "Source");
    source.turns = vec![
        turn("older", "older user", "older assistant"),
        turn("newest", &"🦀".repeat(4_097), "assistant"),
    ];
    let source_revision = store.save_revisioned(&source, None).unwrap();

    let error = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap_err();

    assert_eq!(error.kind(), HistoryStoreErrorKind::ContinuationTooLarge);
    assert_eq!(error.to_string(), LATEST_EXCHANGE_TOO_LARGE);
    assert!(store.get("branch").unwrap().is_none());
}

#[test]
fn stops_at_an_oversized_older_gap_without_scanning_past_it() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let mut source = conversation("source", 10, "Source");
    source.turns = vec![
        turn("oldest", "must not be scanned", "must not be selected"),
        turn("gap", "gap", &"界".repeat(5_462)),
        turn("newest", "newest user", "newest assistant"),
    ];
    let source_revision = store.save_revisioned(&source, None).unwrap();

    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    assert_eq!(prepared.seed, vec![seed("newest user", "newest assistant")]);
}

#[test]
fn stops_before_an_older_pair_that_would_exceed_the_aggregate_budget() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let newest_user = "n".repeat(16_384);
    let newest_assistant = "a".repeat(16_380);
    let mut source = conversation("source", 10, "Source");
    source.turns = vec![
        turn("oldest", "must not be scanned", "must not be selected"),
        turn("gap", "123", "456"),
        turn("newest", &newest_user, &newest_assistant),
    ];
    let source_revision = store.save_revisioned(&source, None).unwrap();

    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    assert_eq!(prepared.seed, vec![seed(&newest_user, &newest_assistant)]);
}

#[test]
fn rejects_a_source_without_eligible_context() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let mut source = conversation("source", 10, "Source");
    source.turns = vec![
        turn_with_state("streaming", "user", "assistant", TurnState::Streaming),
        turn("blank", "user", " \n "),
    ];
    let source_revision = store.save_revisioned(&source, None).unwrap();

    assert_error(
        store.prepare_continuation("source", source_revision, 20, "branch", "operation"),
        NO_ELIGIBLE_CONTEXT,
    );
    assert!(store.get("branch").unwrap().is_none());
}

#[test]
fn source_conflict_and_delete_first_do_not_create_a_branch() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let source = conversation("source", 10, "Source");
    let source_revision = store.save_revisioned(&source, None).unwrap();

    assert_error(
        store.prepare_continuation("source", revision(2), 20, "conflict-branch", "operation-1"),
        REVISION_CONFLICT,
    );
    assert!(store.get("conflict-branch").unwrap().is_none());

    store.delete_revisioned("source", source_revision).unwrap();
    assert_error(
        store.prepare_continuation(
            "source",
            source_revision,
            20,
            "deleted-branch",
            "operation-2",
        ),
        NOT_FOUND,
    );
    assert!(store.get("deleted-branch").unwrap().is_none());
}

#[test]
fn prepare_first_then_source_delete_preserves_the_copied_branch() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("history.sqlite3");
    let store = ConversationHistoryStore::open(&database).unwrap();
    let mut source = conversation("source", 10, "Source");
    source.turns = vec![turn("source-turn", "copied user", "copied assistant")];
    let source_revision = store.save_revisioned(&source, None).unwrap();
    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    store.delete_revisioned("source", source_revision).unwrap();
    drop(store);
    let reopened = ConversationHistoryStore::open(&database).unwrap();

    assert!(reopened.get("source").unwrap().is_none());
    assert_eq!(reopened.get("branch").unwrap().unwrap(), prepared.branch);
    assert_eq!(prepared.seed, vec![seed("copied user", "copied assistant")]);
}

#[test]
fn continuation_title_truncation_preserves_a_utf8_boundary() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let title = "界".repeat(85);
    assert_eq!(title.len(), 255);
    let source = conversation("source", 10, &title);
    let source_revision = store.save_revisioned(&source, None).unwrap();

    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    assert!(prepared.branch.title.starts_with("Continued: "));
    assert_eq!(prepared.branch.title.len(), 254);
    assert!(prepared
        .branch
        .title
        .is_char_boundary(prepared.branch.title.len()));
    assert_eq!(
        prepared.branch.title,
        format!("Continued: {}", "界".repeat(81))
    );
}

#[test]
fn continuation_state_changes_are_revisioned_and_idempotent() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let source_revision = store
        .save_revisioned(&conversation("source", 10, "Source"), None)
        .unwrap();
    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();

    let confirmed_revision = store
        .set_continuation_state(
            "branch",
            prepared.branch.revision,
            ContinuationState::Confirmed,
        )
        .unwrap();
    assert_eq!(confirmed_revision, revision(2));
    assert_eq!(
        store.get("branch").unwrap().unwrap().continuation_state,
        Some(ContinuationState::Confirmed)
    );
    assert_eq!(
        store
            .set_continuation_state(
                "branch",
                prepared.branch.revision,
                ContinuationState::Confirmed,
            )
            .unwrap(),
        confirmed_revision
    );
    assert_error(
        store.set_continuation_state("branch", confirmed_revision, ContinuationState::Unconfirmed),
        "conversation continuation state transition is invalid",
    );
    assert_error(
        store.set_continuation_state("source", source_revision, ContinuationState::Confirmed),
        "conversation is not a continuation branch",
    );

    let retry = store
        .prepare_continuation("source", source_revision, 30, "retry", "operation-2")
        .unwrap();
    let unconfirmed_revision = store
        .set_continuation_state(
            "retry",
            retry.branch.revision,
            ContinuationState::Unconfirmed,
        )
        .unwrap();
    assert_eq!(unconfirmed_revision, revision(2));
    let recovered_revision = store
        .set_continuation_state("retry", unconfirmed_revision, ContinuationState::Confirmed)
        .unwrap();
    assert_eq!(recovered_revision, revision(3));
}

#[test]
fn ordinary_insert_rejects_native_owned_continuation_data() {
    for mutation in 0..4 {
        let temporary = tempfile::tempdir().unwrap();
        let store =
            ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
        let mut value = conversation("new", 10, "New");
        match mutation {
            0 => value.continued_from_id = Some("source".to_owned()),
            1 => value.continuation_operation_id = Some("operation".to_owned()),
            2 => value.continuation_state = Some(ContinuationState::Preparing),
            3 => value.turns[0].origin = ConversationOrigin::ContinuedContext,
            _ => unreachable!(),
        }

        assert_error(
            store.save_revisioned(&value, None),
            INVALID_CONTINUATION_WRITE,
        );
        assert!(store.get("new").unwrap().is_none());
    }
}

#[test]
fn branch_update_rejects_current_revision_provenance_and_state_mutations() {
    for mutation in 0..3 {
        let temporary = tempfile::tempdir().unwrap();
        let store =
            ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
        let source_revision = store
            .save_revisioned(&conversation("source", 10, "Source"), None)
            .unwrap();
        let prepared = store
            .prepare_continuation("source", source_revision, 20, "branch", "operation")
            .unwrap();
        let mut write = prepared.branch.clone();
        match mutation {
            0 => write.continued_from_id = Some("other-source".to_owned()),
            1 => write.continuation_operation_id = Some("other-operation".to_owned()),
            2 => write.continuation_state = Some(ContinuationState::Confirmed),
            _ => unreachable!(),
        }

        assert_error(
            store.save_revisioned(&write, Some(prepared.branch.revision)),
            INVALID_CONTINUATION_WRITE,
        );
        assert_eq!(store.get("branch").unwrap().unwrap(), prepared.branch);
    }
}

#[test]
fn revisioned_update_rejects_a_current_revision_creation_timestamp_mutation() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let original = conversation("existing", 10, "Existing");
    let inserted = store.save_revisioned(&original, None).unwrap();
    let mut write = original.clone();
    write.created_at_ms = 2;

    assert_error(
        store.save_revisioned(&write, Some(inserted)),
        "conversation history creation timestamp is immutable",
    );
    assert_eq!(
        store.get("existing").unwrap().unwrap(),
        source_with_revision(original, inserted)
    );
}

#[test]
fn branch_update_rejects_current_revision_copied_context_mutations() {
    for mutation in 0..3 {
        let temporary = tempfile::tempdir().unwrap();
        let store =
            ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
        let mut source = conversation("source", 10, "Source");
        source.turns = vec![
            turn("one", "first user", "first assistant"),
            turn("two", "second user", "second assistant"),
        ];
        let source_revision = store.save_revisioned(&source, None).unwrap();
        let prepared = store
            .prepare_continuation("source", source_revision, 20, "branch", "operation")
            .unwrap();
        let mut write = prepared.branch.clone();
        match mutation {
            0 => write.turns[0].transcript = "rewritten user".to_owned(),
            1 => {
                write.turns.remove(0);
            }
            2 => write.turns.push(ConversationHistoryTurn {
                turn_id: "injected".to_owned(),
                transcript: "injected user".to_owned(),
                response: "injected assistant".to_owned(),
                state: TurnState::Completed,
                failure_message: None,
                origin: ConversationOrigin::ContinuedContext,
            }),
            _ => unreachable!(),
        }

        assert_error(
            store.save_revisioned(&write, Some(prepared.branch.revision)),
            INVALID_CONTINUATION_WRITE,
        );
        assert_eq!(store.get("branch").unwrap().unwrap(), prepared.branch);
    }
}

#[test]
fn branch_update_preserves_copied_context_and_accepts_a_live_tail() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let source_revision = store
        .save_revisioned(&conversation("source", 10, "Source"), None)
        .unwrap();
    let prepared = store
        .prepare_continuation("source", source_revision, 20, "branch", "operation")
        .unwrap();
    let mut write = prepared.branch.clone();
    write.title = "Renamed branch".to_owned();
    write.updated_at_ms = 30;
    write.turns.push(turn("live", "new user", "new assistant"));

    let saved_revision = store
        .save_revisioned(&write, Some(prepared.branch.revision))
        .unwrap();
    let saved = store.get("branch").unwrap().unwrap();

    assert_eq!(saved_revision, revision(2));
    assert_eq!(saved.revision, saved_revision);
    assert_eq!(saved.title, "Renamed branch");
    assert_eq!(saved.updated_at_ms, 30);
    assert_eq!(saved.created_at_ms, prepared.branch.created_at_ms);
    assert_eq!(saved.continued_from_id, prepared.branch.continued_from_id);
    assert_eq!(
        saved.continuation_operation_id,
        prepared.branch.continuation_operation_id
    );
    assert_eq!(saved.continuation_state, prepared.branch.continuation_state);
    assert_eq!(saved.turns[0], prepared.branch.turns[0]);
    assert_eq!(saved.turns[1], turn("live", "new user", "new assistant"));
}

#[test]
fn failed_legacy_migration_rolls_back_every_schema_change() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("history.sqlite3");
    create_legacy_schema_zero_with_conflicting_column(&database);

    assert!(ConversationHistoryStore::open(&database).is_err());

    let connection = Connection::open(&database).unwrap();
    let columns = table_columns(&connection, "conversations");
    assert_eq!(user_version(&database), 0);
    assert!(!columns.iter().any(|column| column == "revision"));
    assert_eq!(
        columns
            .iter()
            .filter(|column| column.as_str() == "continued_from_id")
            .count(),
        1
    );
}

fn create_legacy_schema_zero(database: &std::path::Path) {
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE conversations (
               id TEXT PRIMARY KEY NOT NULL,
               title TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE conversation_turns (
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
             CREATE INDEX conversations_updated
               ON conversations(updated_at_ms DESC, id ASC);",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO conversations (id, title, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params!["legacy", "Legacy title", 10, 20],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO conversation_turns
             (conversation_id, position, turn_id, transcript, response, state, failure_message)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                "legacy",
                0,
                "legacy-turn",
                "Legacy question\nline two",
                "Legacy answer\nline two",
                "completed",
                Option::<String>::None
            ],
        )
        .unwrap();
}

fn create_legacy_schema_zero_with_conflicting_column(database: &std::path::Path) {
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE conversations (
               id TEXT PRIMARY KEY NOT NULL,
               title TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               updated_at_ms INTEGER NOT NULL,
               continued_from_id TEXT
             );
             CREATE TABLE conversation_turns (
               conversation_id TEXT NOT NULL,
               position INTEGER NOT NULL,
               turn_id TEXT NOT NULL,
               transcript TEXT NOT NULL,
               response TEXT NOT NULL,
               state TEXT NOT NULL,
               failure_message TEXT,
               PRIMARY KEY (conversation_id, position),
               FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
             );",
        )
        .unwrap();
}

fn table_columns(connection: &Connection, table: &str) -> Vec<String> {
    let mut statement = connection
        .prepare(&format!("PRAGMA table_info({table})"))
        .unwrap();
    statement
        .query_map([], |row| row.get(1))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn user_version(database: &std::path::Path) -> i64 {
    Connection::open(database)
        .unwrap()
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap()
}

fn assert_error<T: std::fmt::Debug>(result: Result<T, impl std::fmt::Display>, expected: &str) {
    assert_eq!(result.unwrap_err().to_string(), expected);
}

fn revision(value: u64) -> HistoryRevision {
    HistoryRevision::new(value).unwrap()
}

fn seed(user: &str, assistant: &str) -> ConversationContextExchange {
    ConversationContextExchange {
        user: user.to_owned(),
        assistant: assistant.to_owned(),
    }
}

fn source_with_revision(
    mut source: ConversationHistory,
    revision: HistoryRevision,
) -> ConversationHistory {
    source.revision = revision;
    source
}

fn conversation(id: &str, updated_at_ms: i64, title: &str) -> ConversationHistory {
    ConversationHistory {
        id: id.to_owned(),
        title: title.to_owned(),
        created_at_ms: 1,
        updated_at_ms,
        revision: revision(1),
        continued_from_id: None,
        continuation_operation_id: None,
        continuation_state: None,
        turns: vec![turn("1", title, "Local answer")],
    }
}

fn turn(turn_id: &str, transcript: &str, response: &str) -> ConversationHistoryTurn {
    ConversationHistoryTurn {
        turn_id: turn_id.to_owned(),
        transcript: transcript.to_owned(),
        response: response.to_owned(),
        state: TurnState::Completed,
        failure_message: None,
        origin: ConversationOrigin::Live,
    }
}

fn turn_with_state(
    turn_id: &str,
    transcript: &str,
    response: &str,
    state: TurnState,
) -> ConversationHistoryTurn {
    ConversationHistoryTurn {
        state,
        ..turn(turn_id, transcript, response)
    }
}
