use conversation_desktop::history_store::{
    ConversationHistory, ConversationHistoryStore, ConversationHistoryTurn, TurnState,
};

#[test]
fn saves_reopens_lists_and_deletes_conversations() {
    let temporary = tempfile::tempdir().unwrap();
    let database = temporary.path().join("nested/conversations.sqlite3");
    let store = ConversationHistoryStore::open(&database).unwrap();

    store
        .save(&conversation("first", 10, "First conversation"))
        .unwrap();
    store
        .save(&conversation("second", 20, "Second conversation"))
        .unwrap();

    let reopened = ConversationHistoryStore::open(&database).unwrap();
    let summaries = reopened.list().unwrap();
    assert_eq!(summaries.len(), 2);
    assert_eq!(summaries[0].id, "second");
    assert_eq!(summaries[1].id, "first");
    assert_eq!(
        reopened.get("first").unwrap().unwrap(),
        conversation("first", 10, "First conversation")
    );

    reopened.delete("first").unwrap();
    assert!(reopened.get("first").unwrap().is_none());
    assert_eq!(reopened.list().unwrap().len(), 1);
}

#[test]
fn upsert_replaces_turns_transactionally() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let mut value = conversation("same", 10, "Initial title");
    store.save(&value).unwrap();

    value.title = "Updated title".to_owned();
    value.updated_at_ms = 30;
    value.turns = vec![ConversationHistoryTurn {
        turn_id: "2".to_owned(),
        transcript: "Replacement question".to_owned(),
        response: "Replacement answer".to_owned(),
        state: TurnState::Completed,
        failure_message: None,
    }];
    store.save(&value).unwrap();

    assert_eq!(store.get("same").unwrap().unwrap(), value);
}

#[test]
fn rejects_invalid_history_without_mutating_existing_data() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let original = conversation("valid", 10, "Valid title");
    store.save(&original).unwrap();
    let mut invalid = original.clone();
    invalid.title = " ".to_owned();

    assert!(store.save(&invalid).is_err());
    assert_eq!(store.get("valid").unwrap().unwrap(), original);
}

#[test]
fn preserves_multiline_conversation_content() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    let mut value = conversation("multiline", 10, "Multiline conversation");
    value.turns[0].transcript = "First line\nSecond line".to_owned();
    value.turns[0].response = "Answer one\nAnswer two".to_owned();

    store.save(&value).unwrap();

    assert_eq!(store.get("multiline").unwrap().unwrap(), value);
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

    assert_eq!(encoded["createdAtMs"], 1);
    assert_eq!(encoded["turns"][0]["turnId"], "1");
    assert_eq!(
        encoded["turns"][0]["failureMessage"],
        serde_json::Value::Null
    );
}

#[test]
fn lists_every_saved_conversation() {
    let temporary = tempfile::tempdir().unwrap();
    let store = ConversationHistoryStore::open(&temporary.path().join("history.sqlite3")).unwrap();
    for index in 0..201 {
        store
            .save(&conversation(
                &format!("conversation-{index}"),
                index + 1,
                &format!("Conversation {index}"),
            ))
            .unwrap();
    }

    let summaries = store.list().unwrap();

    assert_eq!(summaries.len(), 201);
    assert_eq!(summaries[0].id, "conversation-200");
    assert_eq!(summaries[200].id, "conversation-0");
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
                    .save(&conversation(
                        &format!("thread-{index}"),
                        index + 1,
                        &format!("Thread {index}"),
                    ))
                    .unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }

    assert_eq!(store.list().unwrap().len(), 4);
}

fn conversation(id: &str, updated_at_ms: i64, title: &str) -> ConversationHistory {
    ConversationHistory {
        id: id.to_owned(),
        title: title.to_owned(),
        created_at_ms: 1,
        updated_at_ms,
        turns: vec![ConversationHistoryTurn {
            turn_id: "1".to_owned(),
            transcript: title.to_owned(),
            response: "Local answer".to_owned(),
            state: TurnState::Completed,
            failure_message: None,
        }],
    }
}
