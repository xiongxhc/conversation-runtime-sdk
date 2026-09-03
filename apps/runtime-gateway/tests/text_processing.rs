//! Proves the batch structured text-processing contract: one call, in the session's pinned
//! profile, that turns instructions plus text into JSON the runtime has validated against a
//! caller-supplied schema.
//!
//! The call creates no conversation turn. It mints synthetic turn and generation identifiers
//! purely to satisfy the generation port's types, and the strongest test here is the one that
//! captures the whole conversation state either side of a batch of calls and asserts it did not
//! move.
//!
//! Every provider in this file is a scripted loopback `TcpListener`. The only credential
//! anywhere is a fixed fake string this repository owns.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    GenerationLanguageModel, GenerationLanguageRequest, MockGenerationLanguageModel, OllamaConfig,
    OllamaLanguageModel, OpenAiLanguageConfig, OpenAiLanguageModel,
};
use conversation_protocol::{
    ConversationContextExchange, ConversationMode, PersonaProfile, ResponseControls, RuntimeStatus,
    MAX_CLIENT_FRAME_BYTES,
};
use conversation_runtime::{
    process_text, ConversationContext, ConversationQualityController, TextProcessingRequest,
    TextProcessingWarning, TextTurnRuntime, MAX_TEXT_PROCESSING_INPUT_BYTES,
    MIN_TEXT_PROCESSING_TURN_ID,
};
use conversation_runtime_gateway::{FrameReader, GatewaySession};
use serde_json::{json, Value};
use tokio::io::{duplex, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// A fixed non-secret string. Nothing here reaches a real provider.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_TEXT_PROCESSING_CREDENTIAL";
const MODEL: &str = "configured-online-text-model";
const TEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The meeting text the caller hands in. No diagnostic anywhere may echo it.
const PRIVATE_INPUT: &str = "private meeting transcript nobody outside may read";
const INSTRUCTIONS: &str = "Summarise the meeting into decisions and owners.";

fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["decisions"],
        "properties": {
            "decisions": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["what", "owner"],
                    "properties": {
                        "what": { "type": "string" },
                        "owner": { "type": "string" }
                    }
                }
            }
        }
    })
}

fn valid_output() -> Value {
    json!({"decisions": [{"what": "ship the beta", "owner": "cluster-1"}]})
}

fn request() -> TextProcessingRequest {
    TextProcessingRequest::new(INSTRUCTIONS, PRIVATE_INPUT, schema().to_string(), 4096)
        .expect("the fixture request is within every bound")
}

// --- the online adapter, end to end ----------------------------------------------------------

#[tokio::test]
async fn schema_valid_output_is_returned_once_with_no_warnings() {
    set_credential();
    let server =
        LanguageServer::start([Reply::sse([content(&valid_output().to_string()), done()])]).await;

    let processed = process_text(
        online(server.endpoint()),
        request(),
        CancellationToken::new(),
    )
    .await
    .expect("a schema-valid answer is accepted");

    assert_eq!(
        serde_json::from_str::<Value>(processed.output()).unwrap(),
        valid_output()
    );
    assert_eq!(processed.warnings(), &[]);
    assert_eq!(server.request_count().await, 1);

    // The instructions travel as the system message and the input as the final user message,
    // and the schema travels as the provider's own structured-output directive rather than as
    // prose inside the prompt.
    let recorded = server.request(0).await;
    assert_eq!(recorded.target, "/chat/completions");
    assert_eq!(recorded.payload["messages"][0]["role"], "system");
    assert_eq!(recorded.payload["messages"][0]["content"], INSTRUCTIONS);
    assert_eq!(recorded.payload["messages"][1]["role"], "user");
    assert_eq!(recorded.payload["messages"][1]["content"], PRIVATE_INPUT);
    assert_eq!(recorded.payload["response_format"]["type"], "json_schema");
    assert_eq!(
        recorded.payload["response_format"]["json_schema"]["schema"],
        schema()
    );
    assert_eq!(
        recorded.payload["response_format"]["json_schema"]["strict"],
        true
    );
}

#[tokio::test]
async fn schema_invalid_output_is_retried_exactly_once_and_the_retry_can_succeed() {
    set_credential();
    let server = LanguageServer::start([
        Reply::sse([content(r#"{"decisions": "not an array"}"#), done()]),
        Reply::sse([content(&valid_output().to_string()), done()]),
    ])
    .await;

    let processed = process_text(
        online(server.endpoint()),
        request(),
        CancellationToken::new(),
    )
    .await
    .expect("the single retry is accepted when it matches the schema");

    assert_eq!(
        serde_json::from_str::<Value>(processed.output()).unwrap(),
        valid_output()
    );
    assert_eq!(
        processed.warnings(),
        &[TextProcessingWarning::SchemaRetried]
    );
    assert_eq!(
        server.request_count().await,
        2,
        "one attempt and exactly one retry"
    );
}

#[tokio::test]
async fn schema_invalid_output_is_retried_once_then_rejected_with_a_redacted_reason() {
    set_credential();
    // Both answers carry the caller's own text back, which is the material a naive diagnostic
    // would quote.
    let leaking = json!({"decisions": PRIVATE_INPUT, "echo": CREDENTIAL_VALUE}).to_string();
    let server = LanguageServer::start([
        Reply::sse([content(&leaking), done()]),
        Reply::sse([content(&leaking), done()]),
    ])
    .await;

    let error = process_text(
        online(server.endpoint()),
        request(),
        CancellationToken::new(),
    )
    .await
    .expect_err("two schema-invalid answers are refused");

    // The count first, so a run that retried more than once says so rather than failing on a
    // knock-on symptom.
    assert_eq!(
        server.request_count().await,
        2,
        "exactly one retry, never a second"
    );
    assert_eq!(
        error.message(),
        "text processing output did not match the requested schema"
    );
    assert_no_echo(&format!("{error:?}"));
}

#[tokio::test]
async fn output_that_is_not_json_at_all_is_retried_once_then_refused() {
    set_credential();
    let server = LanguageServer::start([
        Reply::sse([content("Here are the decisions: none."), done()]),
        Reply::sse([content("Still prose."), done()]),
    ])
    .await;

    let error = process_text(
        online(server.endpoint()),
        request(),
        CancellationToken::new(),
    )
    .await
    .expect_err("prose is not schema-valid output");

    assert_eq!(
        error.message(),
        "text processing output did not match the requested schema"
    );
    assert_eq!(server.request_count().await, 2);
}

#[tokio::test]
async fn an_oversized_input_is_refused_by_the_configured_bound_before_any_request() {
    set_credential();
    let server = LanguageServer::start([Reply::sse([content("unreachable"), done()])]).await;

    // A marker rather than filler, so "the diagnostic does not echo the input" is a claim the
    // assertion can actually test.
    const MARKER: &str = "OVERSIZEDINPUTMARKER";
    let oversized = MARKER.repeat(MAX_TEXT_PROCESSING_INPUT_BYTES / MARKER.len() + 1);
    assert!(oversized.len() > MAX_TEXT_PROCESSING_INPUT_BYTES);
    let error = TextProcessingRequest::new(INSTRUCTIONS, &oversized, schema().to_string(), 4096)
        .expect_err("an input past the bound is refused");

    assert_eq!(
        error.message(),
        "text processing input exceeds the configured limit"
    );
    assert!(
        !error.message().contains(MARKER),
        "the diagnostic must not echo the input: {}",
        error.message()
    );
    // The bound is the boundary, not an approximation of it.
    assert!(TextProcessingRequest::new(
        INSTRUCTIONS,
        "x".repeat(MAX_TEXT_PROCESSING_INPUT_BYTES),
        schema().to_string(),
        4096
    )
    .is_ok());
    assert_eq!(
        server.request_count().await,
        0,
        "a refused request never reaches a provider"
    );
}

#[tokio::test]
async fn a_cancelled_call_stops_without_a_result() {
    set_credential();
    let server = LanguageServer::start([Reply::gated(
        [content(r#"{"decisions": "#)],
        [content("[]}"), done()],
    )])
    .await;
    let cancellation = CancellationToken::new();

    let call = tokio::spawn(process_text(
        online(server.endpoint()),
        request(),
        cancellation.clone(),
    ));
    server.wait_for_request().await;
    cancellation.cancel();
    server.release();

    let error = timeout(TEST_TIMEOUT, call)
        .await
        .expect("a cancelled call must finish")
        .expect("the call task must not panic")
        .expect_err("a cancelled call produces no result");
    assert_eq!(error.message(), "text processing was cancelled");
    assert_eq!(
        server.request_count().await,
        1,
        "cancellation is not a schema failure and must not retry"
    );
}

#[tokio::test]
async fn a_provider_outage_fails_the_call_without_a_retry_or_an_echo() {
    set_credential();
    let server = LanguageServer::start([Reply::failure(
        503,
        format!("{PRIVATE_INPUT} {CREDENTIAL_VALUE} upstream is down"),
    )])
    .await;

    let error = process_text(
        online(server.endpoint()),
        request(),
        CancellationToken::new(),
    )
    .await
    .expect_err("a provider outage fails the call");

    assert_no_echo(&format!("{error:?}"));
    assert_eq!(
        server.request_count().await,
        1,
        "a transport failure is not a schema failure and must not retry"
    );
}

#[tokio::test]
async fn an_answer_larger_than_the_caller_allowed_is_refused() {
    set_credential();
    let large = json!({"decisions": [{"what": "x".repeat(512), "owner": "o"}]}).to_string();
    let server = LanguageServer::start([
        Reply::sse([content(&large), done()]),
        Reply::sse([content(&large), done()]),
    ])
    .await;

    let bounded = TextProcessingRequest::new(INSTRUCTIONS, PRIVATE_INPUT, schema().to_string(), 64)
        .expect("a small output bound is a valid request");
    let error = process_text(online(server.endpoint()), bounded, CancellationToken::new())
        .await
        .expect_err("an answer past the caller's bound is refused");

    assert_eq!(
        error.message(),
        "text processing output exceeded the requested limit"
    );
    assert_no_echo(&format!("{error:?}"));
}

#[tokio::test]
async fn a_schema_the_runtime_cannot_enforce_is_refused_before_any_request() {
    for unusable in [
        // A keyword outside the supported subset would be silently unenforced.
        json!({"type": "object", "patternProperties": {"^a": {"type": "string"}}}),
        // Not a schema object at all.
        json!(["type", "object"]),
        json!("object"),
        // A type this validator cannot check.
        json!({"type": "geometry"}),
        // `required` must name properties as strings.
        json!({"type": "object", "required": [7]}),
    ] {
        let error =
            TextProcessingRequest::new(INSTRUCTIONS, PRIVATE_INPUT, unusable.to_string(), 4096)
                .expect_err("an unenforceable schema is refused");
        assert_eq!(
            error.message(),
            "text processing output schema is not a supported JSON Schema"
        );
    }
}

// --- the local model, behind the same port ----------------------------------------------------

#[tokio::test]
async fn the_local_model_satisfies_the_same_contract() {
    let server = OllamaServer::start([valid_output().to_string()]).await;

    let processed = process_text(
        local(server.endpoint()),
        request(),
        CancellationToken::new(),
    )
    .await
    .expect("the offline model answers the same contract");

    assert_eq!(
        serde_json::from_str::<Value>(processed.output()).unwrap(),
        valid_output()
    );
    assert_eq!(processed.warnings(), &[]);

    // The local model carries the schema in its own structured-output field, so an offline
    // deployment gets the same enforcement rather than a schema pasted into the prompt.
    let recorded = server.request(0).await;
    assert_eq!(recorded["format"], schema());
    assert_eq!(recorded["messages"][0]["role"], "system");
    assert_eq!(recorded["messages"][0]["content"], INSTRUCTIONS);
    assert_eq!(recorded["messages"][1]["content"], PRIVATE_INPUT);
}

// --- the property the whole task rests on -----------------------------------------------------

#[tokio::test]
async fn text_processing_leaves_the_conversation_context_and_turn_counters_untouched() {
    let without = conversation_trace(false).await;
    let with = conversation_trace(true).await;

    // Every field of every request the turn runtime made: the turn identifier, the generation
    // identifier, the seeded history, the resolved quality decision and the transcript. A call
    // that advanced the turn sequence would move the second turn's identifiers; a call that
    // touched the conversation would move its history.
    assert_eq!(
        with.turn_requests, without.turn_requests,
        "text processing moved the conversation the turn runtime sees"
    );
    assert_eq!(with.persona, without.persona);
    assert_eq!(with.active_turn_after, None);
    assert_eq!(without.active_turn_after, None);

    // The synthetic identifiers exist, are distinct, and live in the reserved band no
    // conversation turn can reach. Two of the three calls reached the model: a call cancelled
    // before it starts never asks a provider anything.
    assert_eq!(with.processing_requests.len(), 2);
    let mut synthetic = Vec::new();
    for request in &with.processing_requests {
        assert!(
            request.turn_id().get() >= MIN_TEXT_PROCESSING_TURN_ID,
            "a synthetic turn identifier escaped its reserved band: {}",
            request.turn_id()
        );
        assert_eq!(request.turn_id().get(), request.generation_id().get());
        synthetic.push(request.turn_id().get());
    }
    synthetic.sort_unstable();
    synthetic.dedup();
    assert_eq!(synthetic.len(), 2, "synthetic identifiers must not repeat");
    assert!(
        without.processing_requests.is_empty(),
        "the control run must make no processing call"
    );
}

struct ConversationTrace {
    turn_requests: Vec<GenerationLanguageRequest>,
    processing_requests: Vec<GenerationLanguageRequest>,
    persona: (PersonaProfile, ConversationMode),
    active_turn_after: Option<conversation_runtime::ConversationTurnIdentity>,
}

/// Runs the same conversation twice — once with a batch of text-processing calls wedged between
/// its two turns, once without — and reports everything the turn runtime saw.
async fn conversation_trace(process: bool) -> ConversationTrace {
    let turn_model = Arc::new(MockGenerationLanguageModel::new(["the answer"]));
    let processing_model = Arc::new(MockGenerationLanguageModel::new([
        valid_output().to_string()
    ]));
    let context = test_context();
    let runtime = TextTurnRuntime::new(context.clone(), turn_model.clone());

    context
        .seed_completed_history(
            "trace-seed",
            &[ConversationContextExchange::new("seeded user", "seeded assistant").unwrap()],
        )
        .await
        .expect("history seeds");
    run_turn(&runtime, "first question").await;

    if process {
        // A success, a refusal, and a cancellation: every exit the call has.
        process_text(
            processing_model.clone(),
            request(),
            CancellationToken::new(),
        )
        .await
        .expect("the mock answers the schema");
        let strict =
            TextProcessingRequest::new(INSTRUCTIONS, PRIVATE_INPUT, schema().to_string(), 4)
                .expect("a tiny output bound is a valid request");
        process_text(processing_model.clone(), strict, CancellationToken::new())
            .await
            .expect_err("the answer is past the caller's bound");
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        process_text(processing_model.clone(), request(), cancelled)
            .await
            .expect_err("a cancelled call produces no result");
    }

    run_turn(&runtime, "second question").await;

    ConversationTrace {
        turn_requests: turn_model.requests(),
        processing_requests: processing_model.requests(),
        persona: context.persona_snapshot().await,
        active_turn_after: context.active_turn().await,
    }
}

// --- the gateway command ----------------------------------------------------------------------

#[tokio::test]
async fn process_text_answers_over_the_control_channel_at_version_three() {
    let server = OllamaServer::start([valid_output().to_string(), "the answer".to_owned()]).await;
    let mut gateway = Gateway::start(session_for(server.endpoint())).await;

    gateway
        .write(&process_text_frame(3, "pt-1", "1", 4096))
        .await;
    assert!(gateway
        .read_message()
        .await
        .contains(r#""type":"command_accepted""#));

    let processed = gateway.read_message().await;
    let value: Value = serde_json::from_str(&processed).unwrap();
    assert_eq!(value["type"], "text_processed");
    assert_eq!(value["protocol_version"], 3);
    assert_eq!(value["request_id"], "pt-1");
    assert_eq!(value["processed"]["output"], valid_output());
    assert_eq!(value["processed"]["warnings"], json!([]));
    assert_no_echo(&processed);

    // The session's turn counter did not move: the next real turn is still its first. An
    // implementation that reached for the conversation to get an identity would answer 2 here.
    gateway
        .write(
            r#"{"protocol_version":3,"type":"start_turn","request_id":"turn-1","transcript":"and now a real turn"}"#,
        )
        .await;
    let accepted = gateway.read_message().await;
    assert!(
        accepted.contains(r#""type":"command_accepted""#) && accepted.contains(r#""turn_id":"1""#),
        "text processing consumed a conversation turn identifier: {accepted}"
    );
}

#[tokio::test]
async fn a_call_naming_another_session_is_refused() {
    let server = OllamaServer::start([valid_output().to_string()]).await;
    let mut gateway = Gateway::start(session_for(server.endpoint())).await;

    gateway
        .write(&process_text_frame(3, "pt-other", "2", 4096))
        .await;
    let rejection = gateway.read_message().await;
    assert!(
        rejection.contains(r#""type":"command_rejected""#)
            && rejection.contains("text processing was requested for another session"),
        "a call naming a session this gateway does not serve must be refused: {rejection}"
    );
    assert_eq!(
        server.request_count().await,
        0,
        "a refused call never reaches the language model"
    );
}

#[tokio::test]
async fn process_text_is_a_version_three_command_only() {
    let server = OllamaServer::start([valid_output().to_string()]).await;
    for version in [1, 2] {
        let mut gateway = Gateway::start(session_for(server.endpoint())).await;
        gateway
            .write(&process_text_frame(version, "pt-old", "1", 4096))
            .await;
        let rejection = gateway.read_message().await;
        assert!(
            rejection.contains(r#""type":"command_rejected""#),
            "protocol version {version} must not carry a text-processing command: {rejection}"
        );
    }
    assert_eq!(server.request_count().await, 0);
}

fn process_text_frame(
    version: u64,
    request_id: &str,
    session_id: &str,
    max_output_bytes: u64,
) -> String {
    json!({
        "protocol_version": version,
        "type": "process_text",
        "request_id": request_id,
        "session_id": session_id,
        "instructions": INSTRUCTIONS,
        "input": PRIVATE_INPUT,
        "output_schema": schema(),
        "max_output_bytes": max_output_bytes,
    })
    .to_string()
}

fn session_for(endpoint: &str) -> GatewaySession {
    GatewaySession::new(
        TextTurnRuntime::new(test_context(), local(endpoint)),
        status(),
    )
}

fn status() -> RuntimeStatus {
    RuntimeStatus {
        profile: "offline".to_owned(),
        transport: "stdio".to_owned(),
        privacy_mode: "local_only".to_owned(),
        language_location: "local".to_owned(),
        model_id: "test-model".to_owned(),
        memory_enabled: false,
        memory_location: None,
        telemetry_enabled: false,
        capabilities: vec!["text".to_owned(), "conversation_context_seed".to_owned()],
        components: vec![conversation_protocol::ClientComponentDescriptor {
            kind: "language_model".to_owned(),
            execution_location: "local".to_owned(),
            provider_label: "test-language".to_owned(),
        }],
        last_context_seed_operation_id: None,
    }
}

struct Gateway {
    input: tokio::io::DuplexStream,
    output: FrameReader<tokio::io::DuplexStream>,
    _session: JoinHandle<()>,
}

impl Gateway {
    async fn start(session: GatewaySession) -> Self {
        let (input, reader) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let (writer, output) = duplex(MAX_CLIENT_FRAME_BYTES * 2);
        let handle = tokio::spawn(async move {
            let _ = session.run(reader, writer).await;
        });
        let mut gateway = Self {
            input,
            output: FrameReader::new(output),
            _session: handle,
        };
        let ready = gateway.read_message().await;
        assert!(ready.contains(r#""type":"ready""#), "{ready}");
        gateway
    }

    async fn write(&mut self, payload: &str) {
        write_frame(&mut self.input, payload).await;
    }

    async fn read_message(&mut self) -> String {
        let frame = timeout(TEST_TIMEOUT, self.output.read_frame())
            .await
            .expect("the gateway produced no frame before the test timeout")
            .expect("the gateway frame reader failed")
            .expect("the gateway closed before answering");
        String::from_utf8(frame).expect("gateway frames are UTF-8")
    }
}

async fn write_frame(writer: &mut (impl AsyncWrite + Unpin), payload: &str) {
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await
        .unwrap();
    writer.write_all(payload.as_bytes()).await.unwrap();
    writer.flush().await.unwrap();
}

// --- shared fixtures --------------------------------------------------------------------------

fn test_context() -> ConversationContext {
    ConversationContext::new(ConversationQualityController::new(
        PersonaProfile::default(),
        ResponseControls::default(),
        ConversationMode::DirectAnswer,
    ))
}

async fn run_turn(runtime: &TextTurnRuntime, transcript: &str) {
    let started = runtime.start_turn(transcript).await.expect("a turn starts");
    let mut events = started.into_events();
    timeout(TEST_TIMEOUT, async {
        while events.recv().await.is_some() {}
    })
    .await
    .expect("the turn finished");
}

fn online(endpoint: &str) -> Arc<dyn GenerationLanguageModel> {
    Arc::new(OpenAiLanguageModel::new(
        OpenAiLanguageConfig::new(endpoint, MODEL, CREDENTIAL_ENV).expect("valid configuration"),
    ))
}

fn local(endpoint: &str) -> Arc<dyn GenerationLanguageModel> {
    Arc::new(OllamaLanguageModel::new_direct(
        OllamaConfig::new("test-model")
            .unwrap()
            .with_endpoint(endpoint)
            .unwrap(),
    ))
}

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

/// Neither the caller's text nor the credential may appear in anything this contract produces.
fn assert_no_echo(rendered: &str) {
    assert!(
        !rendered.contains(PRIVATE_INPUT),
        "a diagnostic echoed the caller's input: {rendered}"
    );
    assert!(
        !rendered.contains(CREDENTIAL_VALUE),
        "a diagnostic echoed the credential: {rendered}"
    );
}

fn content(text: &str) -> String {
    json!({"choices": [{"delta": {"content": text}}]}).to_string()
}

fn done() -> String {
    "[DONE]".to_owned()
}

// --- the scripted online provider ---------------------------------------------------------

struct RecordedRequest {
    payload: Value,
    target: String,
}

enum Reply {
    Sse(Vec<String>),
    GatedSse {
        before: Vec<String>,
        after: Vec<String>,
    },
    Failure {
        status: u16,
        body: String,
    },
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
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));
        let worker_requests = requests.clone();
        let worker_gate = gate.clone();

        let worker = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let requests = worker_requests.clone();
                let replies = replies.clone();
                let gate = worker_gate.clone();
                connections.push(tokio::spawn(async move {
                    while let Some(request) = read_request(&mut stream).await {
                        requests.lock().await.push(request);
                        let reply = replies
                            .lock()
                            .await
                            .pop_front()
                            .unwrap_or_else(|| Reply::sse([done()]));
                        if !write_reply(&mut stream, reply, &gate).await {
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
        RecordedRequest {
            payload: requests[index].payload.clone(),
            target: requests[index].target.clone(),
        }
    }

    async fn wait_for_request(&self) {
        timeout(TEST_TIMEOUT, async {
            while self.request_count().await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the provider was never asked");
    }
}

impl Drop for LanguageServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn write_reply(stream: &mut TcpStream, reply: Reply, gate: &Notify) -> bool {
    match reply {
        Reply::Sse(events) => {
            write_sse_header(stream).await;
            for event in events {
                if !write_chunk(stream, &format!("data: {event}\n\n")).await {
                    return false;
                }
            }
            stream.write_all(b"0\r\n\r\n").await.is_ok()
        }
        Reply::GatedSse { before, after } => {
            write_sse_header(stream).await;
            for event in before {
                if !write_chunk(stream, &format!("data: {event}\n\n")).await {
                    return false;
                }
            }
            gate.notified().await;
            for event in after {
                if !write_chunk(stream, &format!("data: {event}\n\n")).await {
                    return false;
                }
            }
            stream.write_all(b"0\r\n\r\n").await.is_ok()
        }
        Reply::Failure { status, body } => {
            let header = format!(
                "HTTP/1.1 {status} Provider Error\r\nContent-Length: {}\r\n\r\n",
                body.len()
            );
            stream.write_all(header.as_bytes()).await.is_ok()
                && stream.write_all(body.as_bytes()).await.is_ok()
        }
    }
}

async fn write_sse_header(stream: &mut TcpStream) {
    let _ = stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .await;
}

async fn write_chunk(stream: &mut TcpStream, body: &str) -> bool {
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

// --- the scripted local provider --------------------------------------------------------------

struct OllamaServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<Value>>>,
    worker: JoinHandle<()>,
}

impl OllamaServer {
    async fn start<I, S>(answers: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let answers = Arc::new(Mutex::new(
            answers
                .into_iter()
                .map(Into::into)
                .collect::<VecDeque<String>>(),
        ));
        let worker_requests = requests.clone();

        let worker = tokio::spawn(async move {
            let mut connections = Vec::new();
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let requests = worker_requests.clone();
                let answers = answers.clone();
                connections.push(tokio::spawn(async move {
                    while let Some(request) = read_request(&mut stream).await {
                        requests.lock().await.push(request.payload);
                        let answer = answers
                            .lock()
                            .await
                            .pop_front()
                            .unwrap_or_else(|| "{}".to_owned());
                        let body = format!(
                            "{}\n{}\n",
                            json!({"message": {"content": answer}, "done": false}),
                            json!({"message": {"content": ""}, "done": true})
                        );
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        if stream.write_all(header.as_bytes()).await.is_err()
                            || stream.write_all(body.as_bytes()).await.is_err()
                        {
                            break;
                        }
                    }
                }));
            }
        });

        Self {
            endpoint,
            requests,
            worker,
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    async fn request(&self, index: usize) -> Value {
        self.requests.lock().await[index].clone()
    }
}

impl Drop for OllamaServer {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        if let Some(position) = find(&request, b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    };

    let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
    let target = headers
        .lines()
        .next()?
        .split_whitespace()
        .nth(1)?
        .to_owned();
    let length = headers
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length: ")
                .or_else(|| line.strip_prefix("Content-Length: "))
        })
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    while request.len() < header_end + length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    }

    let payload = serde_json::from_slice(&request[header_end..header_end + length]).ok()?;
    Some(RecordedRequest { payload, target })
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
