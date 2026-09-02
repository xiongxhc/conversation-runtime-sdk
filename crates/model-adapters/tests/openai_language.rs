use std::collections::VecDeque;
use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, GenerationLanguageModel, GenerationLanguageRequest, GenerationTextDelta,
    OpenAiLanguageConfig, OpenAiLanguageModel,
};
use conversation_protocol::{GenerationId, TurnId};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

/// A fixed non-secret string. No test ever reaches a real provider, and no
/// real credential is ever placed in this repository.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_TEXT_CREDENTIAL";
const ABSENT_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_TEXT_CREDENTIAL_ABSENT";
/// The model identifier is configuration. Tests use a placeholder rather than
/// naming any provider model.
const MODEL: &str = "configured-online-text-model";
const PRIVATE_TEXT: &str = "private meeting transcript";
const TURN: TurnId = TurnId::new(7);
const GENERATION: GenerationId = GenerationId::new(8);

#[tokio::test]
async fn incremental_deltas_arrive_in_order_tagged_with_the_request_identity() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([
        content("The "),
        content("agenda "),
        content("is set."),
        done(),
    ])])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let deltas = assert_single_terminal(&items);
    assert_eq!(deltas, vec!["The ", "agenda ", "is set."]);
    let recorded = server.request(0).await;
    assert_eq!(recorded.target, "/v1/chat/completions");
    assert_eq!(recorded.payload["model"], MODEL);
    assert_eq!(recorded.payload["stream"], true);
}

#[tokio::test]
async fn every_delta_carries_the_turn_and_generation_of_the_request() {
    set_credential();
    let server =
        LanguageServer::start([Reply::sse([content("one"), content("two"), done()])]).await;

    let mut deltas = model(server.endpoint()).stream(request(), CancellationToken::new());

    let mut seen = 0;
    while let Some(item) = next(&mut deltas).await {
        let delta = item.unwrap();
        assert_eq!(delta.turn_id(), TURN);
        assert_eq!(delta.generation_id(), GENERATION);
        seen += 1;
    }
    assert_eq!(seen, 2);
}

#[tokio::test]
async fn the_final_event_completes_the_stream_without_an_error() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([content("done"), done()])]).await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let deltas = assert_single_terminal(&items);
    assert_eq!(deltas, vec!["done"]);
    assert!(
        items.iter().all(Result::is_ok),
        "a completed generation must close without a terminal error"
    );
}

#[tokio::test]
async fn an_empty_response_fails_closed() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([done()])]).await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(
        terminal_error(&items).message(),
        "online text generation response was empty"
    );
}

#[tokio::test]
async fn a_response_that_ends_before_completion_fails_closed() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([content("half a sentence")])]).await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let deltas = assert_single_terminal(&items);
    assert_eq!(deltas, vec!["half a sentence"]);
    assert_eq!(
        terminal_error(&items).message(),
        "online text generation response ended before completion"
    );
}

#[tokio::test]
async fn a_rate_limited_request_reports_a_bounded_rate_limit_diagnostic() {
    set_credential();
    let server = LanguageServer::start([Reply::failure(
        429,
        format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE} slow down"),
    )])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let error = terminal_error(&items);
    assert_eq!(
        error.message(),
        "online text generation was rate limited by the provider"
    );
    assert_eq!(error.stage(), None);
}

#[tokio::test]
async fn other_provider_statuses_report_the_status_and_nothing_else() {
    set_credential();
    let server = LanguageServer::start([Reply::failure(
        503,
        format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE}"),
    )])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(
        terminal_error(&items).message(),
        "online text generation request failed with HTTP status 503"
    );
}

#[tokio::test]
async fn a_response_that_never_starts_is_bounded_by_the_start_timeout() {
    set_credential();
    let server = LanguageServer::start([Reply::StallBeforeHeaders]).await;
    let model = configured_model(
        server.endpoint(),
        CREDENTIAL_ENV,
        Duration::from_millis(50),
        Duration::from_secs(30),
    );

    let items = timeout(
        Duration::from_secs(5),
        collect(model.stream(request(), CancellationToken::new())),
    )
    .await
    .expect("the response start timeout did not bound the request");

    assert_eq!(
        terminal_error(&items).message(),
        "online text generation response did not start before the timeout"
    );
}

#[tokio::test]
async fn a_stalled_response_body_is_bounded_by_the_stall_timeout() {
    set_credential();
    let server = LanguageServer::start([Reply::StallAfter(vec![content("first")])]).await;
    let model = configured_model(
        server.endpoint(),
        CREDENTIAL_ENV,
        Duration::from_secs(30),
        Duration::from_millis(50),
    );

    let items = timeout(
        Duration::from_secs(5),
        collect(model.stream(request(), CancellationToken::new())),
    )
    .await
    .expect("the stall timeout did not bound the response body");

    assert_eq!(assert_single_terminal(&items), vec!["first"]);
    assert_eq!(
        terminal_error(&items).message(),
        "online text generation response stalled"
    );
}

#[tokio::test]
async fn a_transport_failure_is_terminal_and_carries_no_provider_detail() {
    set_credential();
    let server = LanguageServer::start([Reply::Reset]).await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(
        terminal_error(&items).message(),
        "online text generation request failed"
    );
}

#[tokio::test]
async fn cancellation_closes_the_stream_without_a_terminal_error() {
    set_credential();
    let server =
        LanguageServer::start([Reply::gated([content("first")], [content("late"), done()])]).await;
    let cancellation = CancellationToken::new();
    let mut deltas = model(server.endpoint()).stream(request(), cancellation.clone());

    assert_eq!(
        next(&mut deltas).await.unwrap().unwrap().delta(),
        "first",
        "the provider's first delta must reach the consumer"
    );
    cancellation.cancel();
    server.release();
    let rest = drain(deltas).await;

    assert!(
        rest.iter().all(Result::is_ok),
        "cancellation must not add a terminal error to the runtime's own"
    );
    assert!(
        !rest
            .iter()
            .any(|item| item.as_ref().is_ok_and(|delta| delta.delta() == "late")),
        "a delta produced after cancellation must not reach the consumer"
    );
}

#[tokio::test]
async fn a_stream_cancelled_before_it_starts_never_reaches_the_provider() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([content("unreachable"), done()])]).await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let items = collect(model(server.endpoint()).stream(request(), cancellation)).await;

    assert!(items.is_empty());
    assert_eq!(server.request_count().await, 0);
}

#[tokio::test]
async fn a_malformed_stream_event_fails_closed() {
    set_credential();
    let server = LanguageServer::start([Reply::raw(format!(
        "data: {{\"choices\": [broken {PRIVATE_TEXT}\n\n"
    ))])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let error = terminal_error(&items);
    assert_eq!(
        error.message(),
        "online text generation returned a malformed response"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
}

#[tokio::test]
async fn an_unrecognized_sse_field_is_ignored_rather_than_rejected() {
    set_credential();
    let server = LanguageServer::start([Reply::raw(format!(
        "surprise: {PRIVATE_TEXT}\ndata: {}\n\ndata: [DONE]\n\n",
        content("kept")
    ))])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(assert_single_terminal(&items), vec!["kept"]);
}

#[tokio::test]
async fn an_event_field_before_each_data_line_produces_the_same_deltas() {
    set_credential();
    let plain =
        LanguageServer::start([Reply::sse([content("The "), content("agenda"), done()])]).await;
    let typed = LanguageServer::start([Reply::raw(format!(
        "event: message\ndata: {}\n\nevent: message\ndata: {}\n\nevent: done\ndata: [DONE]\n\n",
        content("The "),
        content("agenda")
    ))])
    .await;

    let from_plain =
        collect(model(plain.endpoint()).stream(request(), CancellationToken::new())).await;
    let from_typed =
        collect(model(typed.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(assert_single_terminal(&from_plain), vec!["The ", "agenda"]);
    assert_eq!(
        assert_single_terminal(&from_typed),
        assert_single_terminal(&from_plain),
        "a typed SSE stream must produce exactly the deltas an untyped one does"
    );
}

#[tokio::test]
async fn id_and_retry_fields_are_ignored() {
    set_credential();
    let server = LanguageServer::start([Reply::raw(format!(
        "retry: 3000\nid: 42\ndata: {}\n\nid\n: a comment\ndata: [DONE]\n\n",
        content("only the data")
    ))])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(assert_single_terminal(&items), vec!["only the data"]);
}

#[tokio::test]
async fn a_line_that_is_not_valid_sse_field_syntax_fails_closed() {
    set_credential();
    let server = LanguageServer::start([Reply::raw(format!(
        "bad\u{7}field: {PRIVATE_TEXT}\n\ndata: [DONE]\n\n"
    ))])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let error = terminal_error(&items);
    assert_eq!(
        error.message(),
        "online text generation returned a malformed response"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
}

#[tokio::test]
async fn a_provider_error_object_is_reported_without_echoing_it() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([json!({
        "error": {"message": format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE}"), "code": "bad_request"}
    })
    .to_string()])])
    .await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    let error = terminal_error(&items);
    assert_eq!(
        error.message(),
        "online text generation returned a provider error"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains(CREDENTIAL_VALUE));
}

#[tokio::test]
async fn an_oversized_response_is_bounded_and_terminal() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([
        content(&"x".repeat(600)),
        content(&"y".repeat(600)),
        done(),
    ])])
    .await;
    let model = OpenAiLanguageModel::new(
        base_config(server.endpoint(), CREDENTIAL_ENV)
            .with_max_output_bytes(1_000)
            .unwrap(),
    );

    let items = collect(model.stream(request(), CancellationToken::new())).await;

    assert_eq!(
        terminal_error(&items).message(),
        "online text generation response exceeded the configured limit"
    );
}

#[tokio::test]
async fn a_missing_credential_fails_closed_before_any_request() {
    std::env::remove_var(ABSENT_CREDENTIAL_ENV);
    let server = LanguageServer::start([Reply::sse([content("unreachable"), done()])]).await;
    let model = OpenAiLanguageModel::new(base_config(server.endpoint(), ABSENT_CREDENTIAL_ENV));

    let items = collect(model.stream(request(), CancellationToken::new())).await;

    let error = terminal_error(&items);
    assert_eq!(
        error.message(),
        "online text generation credential is not available"
    );
    assert!(
        !error.message().contains(ABSENT_CREDENTIAL_ENV),
        "a diagnostic must not name the variable it read"
    );
    assert_eq!(server.request_count().await, 0);
}

#[tokio::test]
async fn the_credential_travels_only_in_the_authorization_header() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([content("ok"), done()])]).await;

    let items = collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;

    assert_eq!(assert_single_terminal(&items), vec!["ok"]);
    let recorded = server.request(0).await;
    assert_eq!(
        recorded.authorization.as_deref(),
        Some(format!("Bearer {CREDENTIAL_VALUE}").as_str())
    );
    assert!(
        !recorded.payload.to_string().contains(CREDENTIAL_VALUE),
        "the credential must never appear in a request body"
    );
    assert!(
        !recorded.target.contains(CREDENTIAL_VALUE),
        "the credential must never appear in a request target"
    );
}

#[tokio::test]
async fn the_request_carries_the_configured_model_and_the_transcript_as_the_user_turn() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([content("ok"), done()])]).await;
    let model = OpenAiLanguageModel::new(
        base_config(server.endpoint(), CREDENTIAL_ENV)
            .with_system_prompt("Answer briefly.")
            .unwrap()
            .with_temperature(0.2)
            .unwrap()
            .with_max_output_tokens(256)
            .unwrap(),
    );

    let items = collect(model.stream(request(), CancellationToken::new())).await;

    assert_eq!(assert_single_terminal(&items), vec!["ok"]);
    let payload = server.request(0).await.payload;
    assert_eq!(payload["model"], MODEL);
    assert_eq!(payload["temperature"], 0.2);
    assert_eq!(payload["max_completion_tokens"], 256);
    assert_eq!(payload["messages"][0]["role"], "system");
    assert_eq!(payload["messages"][0]["content"], "Answer briefly.");
    assert_eq!(payload["messages"][1]["role"], "user");
    assert_eq!(payload["messages"][1]["content"], PRIVATE_TEXT);
}

#[tokio::test]
async fn every_error_variant_redacts_the_credential_and_the_transcript() {
    set_credential();
    let leak = format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE} Bearer {CREDENTIAL_VALUE}");
    let variants: Vec<(&str, Reply)> = vec![
        ("rate limit", Reply::failure(429, leak.clone())),
        ("http status", Reply::failure(503, leak.clone())),
        (
            "malformed",
            Reply::raw(format!("data: {{not json {leak}\n\n")),
        ),
        (
            "invalid sse syntax",
            Reply::raw(format!("bad\u{7}field: {leak}\n\ndata: [DONE]\n\n")),
        ),
        (
            "provider error",
            Reply::sse([json!({"error": {"message": leak.clone()}}).to_string()]),
        ),
        ("empty", Reply::sse([done()])),
        (
            "ended early",
            Reply::sse([json!({"choices": [{"delta": {"content": leak.clone()}}]}).to_string()]),
        ),
        ("transport", Reply::Reset),
    ];

    for (name, reply) in variants {
        let server = LanguageServer::start([reply]).await;
        let items =
            collect(model(server.endpoint()).stream(request(), CancellationToken::new())).await;
        let error = terminal_error(&items);
        assert!(
            !error.message().contains(CREDENTIAL_VALUE),
            "{name} echoed the credential: {}",
            error.message()
        );
        assert!(
            !error.message().to_ascii_lowercase().contains("bearer"),
            "{name} echoed an authorization header: {}",
            error.message()
        );
        assert!(
            !error.message().contains(PRIVATE_TEXT),
            "{name} echoed the transcript: {}",
            error.message()
        );
        assert!(
            !format!("{error:?}").contains(CREDENTIAL_VALUE),
            "{name} echoed the credential through Debug"
        );
    }
}

#[tokio::test]
async fn a_configuration_error_never_names_the_credential_value() {
    set_credential();
    let error = OpenAiLanguageConfig::new(
        format!("https://user:{CREDENTIAL_VALUE}@example.test/v1"),
        MODEL,
        CREDENTIAL_ENV,
    )
    .unwrap_err();

    assert!(!error.message().contains(CREDENTIAL_VALUE));
    assert_eq!(
        error.message(),
        "invalid online text generation configuration: endpoint must not carry credentials"
    );
}

#[tokio::test]
async fn dropping_the_receiver_stops_the_provider_exchange() {
    set_credential();
    let server =
        LanguageServer::start([Reply::gated([content("first")], [content("late"), done()])]).await;
    let mut deltas = model(server.endpoint()).stream(request(), CancellationToken::new());

    assert_eq!(next(&mut deltas).await.unwrap().unwrap().delta(), "first");
    drop(deltas);
    server.release();

    server.wait_for_disconnect().await;
}

#[tokio::test]
async fn a_cancellation_racing_provider_completion_yields_exactly_one_terminal() {
    set_credential();
    for delay_yields in 0..8 {
        let server =
            LanguageServer::start([Reply::gated([content("first")], [content("last"), done()])])
                .await;
        let cancellation = CancellationToken::new();
        let mut deltas = model(server.endpoint()).stream(request(), cancellation.clone());
        assert_eq!(next(&mut deltas).await.unwrap().unwrap().delta(), "first");

        // Release the provider's completion, then cancel a deterministic
        // number of scheduler turns later, so each iteration pins a different
        // interleaving of completion and interrupt.
        server.release();
        for _ in 0..delay_yields {
            tokio::task::yield_now().await;
        }
        cancellation.cancel();

        let rest = timeout(Duration::from_secs(5), drain(deltas))
            .await
            .unwrap_or_else(|_| panic!("the stream did not close (delay_yields={delay_yields})"));
        let errors = rest.iter().filter(|item| item.is_err()).count();
        assert!(
            errors <= 1,
            "delay_yields={delay_yields} produced {errors} terminal errors"
        );
        if let Some(position) = rest.iter().position(Result::is_err) {
            assert_eq!(
                position,
                rest.len() - 1,
                "delay_yields={delay_yields} continued after a terminal error"
            );
        }
    }
}

#[tokio::test]
async fn a_provider_failure_racing_a_client_disconnect_yields_no_late_error() {
    set_credential();
    let server = LanguageServer::start([Reply::gated(
        [content("first")],
        [json!({"error": {"message": "provider failed"}}).to_string()],
    )])
    .await;
    let mut deltas = model(server.endpoint()).stream(request(), CancellationToken::new());

    assert_eq!(next(&mut deltas).await.unwrap().unwrap().delta(), "first");
    drop(deltas);
    server.release();

    server.wait_for_disconnect().await;
}

#[test]
fn configuration_rejects_unusable_endpoints_credential_names_and_models() {
    assert!(OpenAiLanguageConfig::new("https://example.test/v1", MODEL, "ONLINE_TEXT_KEY").is_ok());
    assert!(OpenAiLanguageConfig::new("ftp://example.test", MODEL, "ONLINE_TEXT_KEY").is_err());
    assert!(OpenAiLanguageConfig::new("not a url", MODEL, "ONLINE_TEXT_KEY").is_err());
    assert!(OpenAiLanguageConfig::new("https://example.test", "", "ONLINE_TEXT_KEY").is_err());
    assert!(OpenAiLanguageConfig::new("https://example.test", MODEL, "").is_err());
    assert!(
        OpenAiLanguageConfig::new("https://example.test", MODEL, "lowercase").is_err(),
        "credential variable names follow the gateway's rule"
    );
    assert!(OpenAiLanguageConfig::new("https://example.test", MODEL, "1LEADING_DIGIT").is_err());

    let config = OpenAiLanguageConfig::new(
        "https://example.test/v1/?token=leaked#fragment",
        MODEL,
        "ONLINE_TEXT_KEY",
    )
    .unwrap();
    assert_eq!(config.endpoint().query(), None);
    assert_eq!(config.endpoint().fragment(), None);
    assert_eq!(config.credential_env(), "ONLINE_TEXT_KEY");
    assert_eq!(config.model(), MODEL);
    assert!(!format!("{config:?}").contains("leaked"));
}

#[test]
fn configuration_bounds_are_validated() {
    let base =
        || OpenAiLanguageConfig::new("https://example.test/v1", MODEL, "ONLINE_TEXT_KEY").unwrap();

    assert!(base().with_temperature(f32::NAN).is_err());
    assert!(base().with_temperature(-0.1).is_err());
    assert!(base().with_temperature(0.0).is_ok());
    assert!(base().with_max_output_tokens(0).is_err());
    assert!(base().with_max_output_bytes(0).is_err());
    assert!(base().with_response_start_timeout(Duration::ZERO).is_err());
    assert!(base().with_stall_timeout(Duration::ZERO).is_err());
    assert!(base().with_system_prompt("  ").is_err());
}

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

fn base_config(endpoint: &str, credential_env: &str) -> OpenAiLanguageConfig {
    OpenAiLanguageConfig::new(format!("{endpoint}/v1"), MODEL, credential_env).unwrap()
}

fn model(endpoint: &str) -> OpenAiLanguageModel {
    OpenAiLanguageModel::new(base_config(endpoint, CREDENTIAL_ENV))
}

fn configured_model(
    endpoint: &str,
    credential_env: &str,
    response_start_timeout: Duration,
    stall_timeout: Duration,
) -> OpenAiLanguageModel {
    OpenAiLanguageModel::new(
        base_config(endpoint, credential_env)
            .with_response_start_timeout(response_start_timeout)
            .unwrap()
            .with_stall_timeout(stall_timeout)
            .unwrap(),
    )
}

fn request() -> GenerationLanguageRequest {
    GenerationLanguageRequest::new(TURN, GENERATION, PRIVATE_TEXT)
}

fn content(text: &str) -> String {
    json!({"choices": [{"delta": {"content": text}, "finish_reason": Value::Null}]}).to_string()
}

fn done() -> String {
    "[DONE]".to_owned()
}

async fn next(
    receiver: &mut mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
) -> Option<Result<GenerationTextDelta, AdapterError>> {
    timeout(Duration::from_secs(5), receiver.recv())
        .await
        .expect("the generation stream produced nothing before the test timeout")
}

async fn drain(
    mut receiver: mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
) -> Vec<Result<GenerationTextDelta, AdapterError>> {
    let mut items = Vec::new();
    while let Some(item) = receiver.recv().await {
        items.push(item);
    }
    items
}

async fn collect(
    receiver: mpsc::Receiver<Result<GenerationTextDelta, AdapterError>>,
) -> Vec<Result<GenerationTextDelta, AdapterError>> {
    timeout(Duration::from_secs(10), drain(receiver))
        .await
        .expect("the generation stream never closed")
}

/// The contract every path must satisfy: the stream is a run of deltas
/// followed by at most one terminal error, and it always closes.
fn assert_single_terminal(items: &[Result<GenerationTextDelta, AdapterError>]) -> Vec<String> {
    let errors = items.iter().filter(|item| item.is_err()).count();
    assert!(errors <= 1, "the stream carried {errors} terminal errors");
    if let Some(position) = items.iter().position(Result::is_err) {
        assert_eq!(
            position,
            items.len() - 1,
            "the stream continued after its terminal error"
        );
    }
    items
        .iter()
        .filter_map(|item| item.as_ref().ok())
        .map(|delta| delta.delta().to_owned())
        .collect()
}

fn terminal_error(items: &[Result<GenerationTextDelta, AdapterError>]) -> AdapterError {
    assert_single_terminal(items);
    items
        .last()
        .expect("the stream closed without any item")
        .as_ref()
        .expect_err("the stream closed without a terminal error")
        .clone()
}

struct RecordedRequest {
    payload: Value,
    target: String,
    authorization: Option<String>,
}

enum Reply {
    Sse(Vec<String>),
    GatedSse {
        before: Vec<String>,
        after: Vec<String>,
    },
    Raw(String),
    Failure {
        status: u16,
        body: String,
    },
    StallAfter(Vec<String>),
    StallBeforeHeaders,
    Reset,
}

impl Reply {
    fn sse<I, S>(events: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::Sse(events.into_iter().map(Into::into).collect())
    }

    fn gated<B, A, S>(before: B, after: A) -> Self
    where
        B: IntoIterator<Item = S>,
        A: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::GatedSse {
            before: before.into_iter().map(Into::into).collect(),
            after: after.into_iter().map(Into::into).collect(),
        }
    }

    fn raw(body: impl Into<String>) -> Self {
        Self::Raw(body.into())
    }

    fn failure(status: u16, body: impl Into<String>) -> Self {
        Self::Failure {
            status,
            body: body.into(),
        }
    }
}

struct LanguageServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    gate: Arc<Notify>,
    disconnected: Arc<Notify>,
    disconnect_seen: Arc<Mutex<bool>>,
    worker: JoinHandle<()>,
}

impl LanguageServer {
    async fn start<I>(replies: I) -> Self
    where
        I: IntoIterator<Item = Reply>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let disconnected = Arc::new(Notify::new());
        let disconnect_seen = Arc::new(Mutex::new(false));
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));
        let worker_requests = requests.clone();
        let worker_gate = gate.clone();
        let worker_disconnected = disconnected.clone();
        let worker_disconnect_seen = disconnect_seen.clone();

        let worker = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let requests = worker_requests.clone();
                let replies = replies.clone();
                let gate = worker_gate.clone();
                let disconnected = worker_disconnected.clone();
                let disconnect_seen = worker_disconnect_seen.clone();
                connections.push(tokio::spawn(async move {
                    while let Some(request) = read_request(&mut stream).await {
                        requests.lock().await.push(request);
                        let reply = replies
                            .lock()
                            .await
                            .pop_front()
                            .unwrap_or_else(|| Reply::sse([done()]));
                        let outcome = write_reply(&mut stream, reply, &gate).await;
                        if outcome == ReplyOutcome::Disconnected {
                            *disconnect_seen.lock().await = true;
                            disconnected.notify_waiters();
                        }
                        if outcome != ReplyOutcome::KeepAlive {
                            break;
                        }
                    }
                }));
            }
        });

        Self {
            endpoint,
            requests,
            gate,
            disconnected,
            disconnect_seen,
            worker,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn release(&self) {
        self.gate.notify_waiters();
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    async fn request(&self, index: usize) -> RecordedRequest {
        let requests = self.requests.lock().await;
        let request = &requests[index];
        RecordedRequest {
            payload: request.payload.clone(),
            target: request.target.clone(),
            authorization: request.authorization.clone(),
        }
    }

    async fn wait_for_disconnect(&self) {
        timeout(Duration::from_secs(5), async {
            loop {
                let notified = self.disconnected.notified();
                if *self.disconnect_seen.lock().await {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("the generation request never disconnected");
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

#[derive(Eq, PartialEq)]
enum ReplyOutcome {
    KeepAlive,
    Close,
    Disconnected,
}

async fn write_reply(stream: &mut TcpStream, reply: Reply, gate: &Notify) -> ReplyOutcome {
    match reply {
        Reply::Sse(events) => {
            write_sse_header(stream).await;
            for event in events {
                write_chunk(stream, &format!("data: {event}\n\n")).await;
            }
            stream.write_all(b"0\r\n\r\n").await.unwrap();
            ReplyOutcome::KeepAlive
        }
        Reply::GatedSse { before, after } => {
            write_sse_header(stream).await;
            for event in before {
                write_chunk(stream, &format!("data: {event}\n\n")).await;
            }
            gate.notified().await;
            for event in after {
                if !write_chunk_checked(stream, &format!("data: {event}\n\n")).await {
                    return ReplyOutcome::Disconnected;
                }
            }
            if stream.write_all(b"0\r\n\r\n").await.is_err() {
                return ReplyOutcome::Disconnected;
            }
            observe_disconnect(stream).await
        }
        Reply::Raw(body) => {
            write_sse_header(stream).await;
            write_chunk(stream, &body).await;
            stream.write_all(b"0\r\n\r\n").await.unwrap();
            ReplyOutcome::KeepAlive
        }
        Reply::Failure { status, body } => {
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Provider Error\r\nContent-Length: {}\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            stream.write_all(body.as_bytes()).await.unwrap();
            ReplyOutcome::KeepAlive
        }
        Reply::StallAfter(events) => {
            write_sse_header(stream).await;
            for event in events {
                write_chunk(stream, &format!("data: {event}\n\n")).await;
            }
            observe_disconnect(stream).await
        }
        Reply::StallBeforeHeaders => observe_disconnect(stream).await,
        Reply::Reset => ReplyOutcome::Close,
    }
}

async fn observe_disconnect(stream: &mut TcpStream) -> ReplyOutcome {
    let mut probe = [0_u8; 1];
    loop {
        match stream.read(&mut probe).await {
            Ok(0) | Err(_) => return ReplyOutcome::Disconnected,
            Ok(_) => {}
        }
    }
}

async fn write_sse_header(stream: &mut TcpStream) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .await
        .unwrap();
}

async fn write_chunk(stream: &mut TcpStream, body: &str) {
    assert!(
        write_chunk_checked(stream, body).await,
        "the client disconnected before the scripted chunk was written"
    );
}

async fn write_chunk_checked(stream: &mut TcpStream, body: &str) -> bool {
    if stream
        .write_all(format!("{:x}\r\n", body.len()).as_bytes())
        .await
        .is_err()
        || stream.write_all(body.as_bytes()).await.is_err()
        || stream.write_all(b"\r\n").await.is_err()
    {
        return false;
    }
    tokio::task::yield_now().await;
    true
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            break header_end;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let content_length = header_value(&headers, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap();
    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    }

    Some(RecordedRequest {
        payload: serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap(),
        target: headers
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap()
            .to_owned(),
        authorization: header_value(&headers, "authorization"),
    })
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        line.split_once(':').and_then(|(header, value)| {
            header
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    })
}
