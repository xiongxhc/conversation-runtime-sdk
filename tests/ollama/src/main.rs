use std::env;
use std::future::Future;
use std::io::{self, Read, Write};
use std::time::Instant;

use conversation_model_adapters::{
    LanguageModelRequest, OllamaChatMetrics, OllamaConfig, OllamaLanguageModel,
};
use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

const DEFAULT_FIRST_DELTA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
const DEFAULT_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const DEFAULT_TOTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
const MAX_TIMEOUT_MILLISECONDS: u64 = u32::MAX as u64 * 1_000;

#[derive(Debug, Eq, PartialEq)]
struct ProbeArguments {
    model: String,
    prompt: String,
}

#[tokio::main]
async fn main() {
    let arguments = match parse_arguments(env::args(), io::stdin().lock()) {
        Ok(arguments) => arguments,
        Err(error) => exit_with_failure("unavailable", ProbeFailure::arguments(error)),
    };
    let endpoint = env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    let model = arguments.model.clone();

    if let Err(failure) = run_probe(arguments, &endpoint).await {
        exit_with_failure(&model, failure);
    }
}

async fn run_probe(arguments: ProbeArguments, endpoint: &str) -> Result<(), ProbeFailure> {
    let request_started_at = Instant::now();
    let timeouts = ProbeTimeouts::from_environment()
        .map_err(|error| ProbeFailure::configuration(error, request_started_at.elapsed()))?;
    run_probe_with_timeouts(arguments, endpoint, timeouts, request_started_at).await
}

async fn run_probe_with_timeouts(
    arguments: ProbeArguments,
    endpoint: &str,
    timeouts: ProbeTimeouts,
    request_started_at: Instant,
) -> Result<(), ProbeFailure> {
    let model = OllamaLanguageModel::new(
        OllamaConfig::new(arguments.model.clone())
            .map_err(|error| ProbeFailure::configuration(error, request_started_at.elapsed()))?
            .with_endpoint(endpoint)
            .map_err(|error| ProbeFailure::configuration(error, request_started_at.elapsed()))?
            .with_thinking(false)
            .with_temperature(0.0)
            .with_seed(42)
            .with_num_predict(128)
            .map_err(|error| ProbeFailure::configuration(error, request_started_at.elapsed()))?
            .with_num_ctx(8192)
            .map_err(|error| ProbeFailure::configuration(error, request_started_at.elapsed()))?,
    );
    let total_deadline = request_started_at
        .checked_add(timeouts.total)
        .map(tokio::time::Instant::from_std)
        .ok_or_else(|| {
            ProbeFailure::configuration(
                "OLLAMA_TOTAL_TIMEOUT_MS exceeds the supported deadline range",
                request_started_at.elapsed(),
            )
        })?;
    let cancellation = CancellationToken::new();
    let mut stream = model.stream_chat(
        LanguageModelRequest::new(TurnId::new(1), arguments.prompt),
        cancellation.clone(),
    );
    let mut first_delta_at = None;

    loop {
        let stage = if first_delta_at.is_some() {
            TimeoutStage::Idle
        } else {
            TimeoutStage::FirstDelta
        };
        let stage_timeout = match stage {
            TimeoutStage::FirstDelta => timeouts.first_delta,
            TimeoutStage::Idle => timeouts.idle,
            TimeoutStage::Total => unreachable!("total timeout uses a shared deadline"),
        };
        match await_next_delta(stream.recv_delta(), total_deadline, stage_timeout, stage).await {
            ReceiveOutcome::Timeout(TimeoutStage::Total) => {
                cancellation.cancel();
                return Err(ProbeFailure::timeout(
                    TimeoutStage::Total,
                    request_started_at.elapsed(),
                ));
            }
            ReceiveOutcome::Timeout(stage) => {
                cancellation.cancel();
                return Err(ProbeFailure::timeout(stage, request_started_at.elapsed()));
            }
            ReceiveOutcome::Delta(delta) => match delta {
                Some(Ok(delta)) => {
                    first_delta_at.get_or_insert_with(Instant::now);
                    print!("{delta}");
                    io::stdout().flush().map_err(|error| {
                        ProbeFailure::output(error, request_started_at.elapsed())
                    })?;
                }
                Some(Err(error)) => {
                    return Err(ProbeFailure::adapter(error, request_started_at.elapsed()));
                }
                None => break,
            },
        }
    }

    let completed_at = Instant::now();
    let first_delta_ms = require_first_delta(first_delta_at)
        .map_err(|error| ProbeFailure::adapter(error, request_started_at.elapsed()))?
        .duration_since(request_started_at);
    let total_ms = completed_at.duration_since(request_started_at);
    let metrics = stream
        .final_metrics()
        .await
        .map_err(|error| ProbeFailure::adapter(error, request_started_at.elapsed()))?;

    eprintln!(
        "{}",
        format_success_report(&arguments.model, first_delta_ms, total_ms, &metrics)
    );

    Ok(())
}

fn parse_arguments(
    arguments: impl IntoIterator<Item = String>,
    mut standard_input: impl Read,
) -> Result<ProbeArguments, String> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let Some(model) = arguments.next().filter(|model| !model.trim().is_empty()) else {
        return Err(usage_error());
    };
    if model.chars().any(char::is_control) {
        return Err("Ollama model identifiers cannot contain control characters".into());
    }
    let prompt = arguments.collect::<Vec<_>>().join(" ");
    let prompt = if prompt.trim().is_empty() {
        let mut standard_input_prompt = String::new();
        standard_input
            .read_to_string(&mut standard_input_prompt)
            .map_err(|error| format!("could not read prompt from standard input: {error}"))?;
        standard_input_prompt.trim().to_owned()
    } else {
        prompt
    };

    if prompt.trim().is_empty() {
        return Err(usage_error());
    }

    Ok(ProbeArguments { model, prompt })
}

fn usage_error() -> String {
    "Usage: conversation-ollama-probe <model> <prompt...>".into()
}

fn require_first_delta(first_delta_at: Option<Instant>) -> Result<Instant, &'static str> {
    first_delta_at.ok_or("Ollama response completed without a text delta")
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProbeTimeouts {
    first_delta: std::time::Duration,
    idle: std::time::Duration,
    total: std::time::Duration,
}

impl ProbeTimeouts {
    const fn defaults() -> Self {
        Self {
            first_delta: DEFAULT_FIRST_DELTA_TIMEOUT,
            idle: DEFAULT_IDLE_TIMEOUT,
            total: DEFAULT_TOTAL_TIMEOUT,
        }
    }

    fn from_environment() -> Result<Self, String> {
        Self::from_millis(
            env::var("OLLAMA_FIRST_DELTA_TIMEOUT_MS").ok().as_deref(),
            env::var("OLLAMA_IDLE_TIMEOUT_MS").ok().as_deref(),
            env::var("OLLAMA_TOTAL_TIMEOUT_MS").ok().as_deref(),
        )
    }

    fn from_millis(
        first_delta: Option<&str>,
        idle: Option<&str>,
        total: Option<&str>,
    ) -> Result<Self, String> {
        let defaults = Self::defaults();
        Ok(Self {
            first_delta: parse_timeout(
                "OLLAMA_FIRST_DELTA_TIMEOUT_MS",
                first_delta,
                defaults.first_delta,
            )?,
            idle: parse_timeout("OLLAMA_IDLE_TIMEOUT_MS", idle, defaults.idle)?,
            total: parse_timeout("OLLAMA_TOTAL_TIMEOUT_MS", total, defaults.total)?,
        })
    }
}

fn parse_timeout(
    name: &str,
    milliseconds: Option<&str>,
    default: std::time::Duration,
) -> Result<std::time::Duration, String> {
    let Some(milliseconds) = milliseconds else {
        return Ok(default);
    };
    let milliseconds = milliseconds
        .parse::<u64>()
        .map_err(|_| format!("{name} must be a non-zero number of milliseconds"))?;
    if milliseconds == 0 {
        return Err(format!("{name} must be a non-zero number of milliseconds"));
    }
    if milliseconds > MAX_TIMEOUT_MILLISECONDS {
        return Err(format!("{name} exceeds the supported deadline range"));
    }

    let timeout = std::time::Duration::from_millis(milliseconds);
    if Instant::now().checked_add(timeout).is_none() {
        return Err(format!("{name} exceeds the supported deadline range"));
    }

    Ok(timeout)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimeoutStage {
    FirstDelta,
    Idle,
    Total,
}

#[derive(Debug, Eq, PartialEq)]
enum ReceiveOutcome {
    Delta(Option<Result<String, conversation_model_adapters::AdapterError>>),
    Timeout(TimeoutStage),
}

async fn await_next_delta<F>(
    next_delta: F,
    total_deadline: tokio::time::Instant,
    stage_timeout: std::time::Duration,
    stage: TimeoutStage,
) -> ReceiveOutcome
where
    F: Future<Output = Option<Result<String, conversation_model_adapters::AdapterError>>>,
{
    let total_timer = tokio::time::sleep_until(total_deadline);
    let stage_timer = tokio::time::sleep(stage_timeout);
    tokio::pin!(total_timer);
    tokio::pin!(stage_timer);

    tokio::select! {
        biased;
        _ = &mut total_timer => ReceiveOutcome::Timeout(TimeoutStage::Total),
        _ = &mut stage_timer => ReceiveOutcome::Timeout(stage),
        delta = next_delta => ReceiveOutcome::Delta(delta),
    }
}

impl TimeoutStage {
    const fn as_str(self) -> &'static str {
        match self {
            Self::FirstDelta => "first_delta",
            Self::Idle => "idle",
            Self::Total => "total",
        }
    }
}

#[derive(Debug)]
struct ProbeFailure {
    stage: &'static str,
    error: String,
    timeout_stage: Option<TimeoutStage>,
    elapsed: Option<std::time::Duration>,
}

impl ProbeFailure {
    fn arguments(error: String) -> Self {
        Self::new("arguments", error, None)
    }

    fn configuration(error: impl ToString, elapsed: std::time::Duration) -> Self {
        Self::new("configuration", error.to_string(), Some(elapsed))
    }

    fn adapter(error: impl ToString, elapsed: std::time::Duration) -> Self {
        Self::new("adapter", error.to_string(), Some(elapsed))
    }

    fn output(error: impl ToString, elapsed: std::time::Duration) -> Self {
        Self::new("output", error.to_string(), Some(elapsed))
    }

    fn timeout(stage: TimeoutStage, elapsed: std::time::Duration) -> Self {
        Self {
            stage: "timeout",
            error: String::new(),
            timeout_stage: Some(stage),
            elapsed: Some(elapsed),
        }
    }

    fn new(stage: &'static str, error: String, elapsed: Option<std::time::Duration>) -> Self {
        Self {
            stage,
            error,
            timeout_stage: None,
            elapsed,
        }
    }
}

fn exit_with_failure(model: &str, failure: ProbeFailure) -> ! {
    eprintln!("{}", format_failure_report(model, failure));
    std::process::exit(1);
}

fn format_failure_report(model: &str, failure: ProbeFailure) -> String {
    let elapsed_ms = failure.elapsed.map_or_else(
        || "unavailable".to_owned(),
        |elapsed| elapsed.as_millis().to_string(),
    );
    match failure.timeout_stage {
        Some(timeout_stage) => format!(
            "model={model}\nstatus=timeout\ntimeout_stage={}\nelapsed_ms={elapsed_ms}",
            timeout_stage.as_str(),
        ),
        None => format!(
            "model={model}\nstatus=error\nstage={}\nelapsed_ms={elapsed_ms}\nerror={}",
            failure.stage,
            sanitize_error(&failure.error),
        ),
    }
}

fn sanitize_error(error: &str) -> String {
    error
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn format_success_report(
    model: &str,
    first_delta: std::time::Duration,
    total: std::time::Duration,
    metrics: &OllamaChatMetrics,
) -> String {
    format!(
        "model={model}\nstatus=ok\nfirst_delta_ms={}\ntotal_ms={}\nthink=false\ntemperature=0\nseed=42\nnum_predict=128\nnum_ctx=8192\nollama_total_duration_ns={}\nollama_load_duration_ns={}\nollama_prompt_eval_count={}\nollama_prompt_eval_duration_ns={}\nollama_eval_count={}\nollama_eval_duration_ns={}",
        first_delta.as_millis(),
        total.as_millis(),
        format_metric(metrics.total_duration_ns()),
        format_metric(metrics.load_duration_ns()),
        format_metric(metrics.prompt_eval_count()),
        format_metric(metrics.prompt_eval_duration_ns()),
        format_metric(metrics.eval_count()),
        format_metric(metrics.eval_duration_ns()),
    )
}

fn format_metric(value: Option<u64>) -> String {
    value.map_or_else(|| "unavailable".to_owned(), |value| value.to_string())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;
    use std::time::Duration;

    use conversation_model_adapters::{AdapterError, OllamaChatMetrics};

    use super::{
        await_next_delta, format_failure_report, format_success_report, parse_arguments,
        require_first_delta, run_probe, ProbeArguments, ProbeFailure, ProbeTimeouts,
        ReceiveOutcome, TimeoutStage,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{mpsc, Mutex};

    #[test]
    fn parses_exact_model_identifier_and_remaining_prompt_words() {
        let arguments = vec![
            "conversation-ollama-probe".to_owned(),
            "hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K".to_owned(),
            "Answer".to_owned(),
            "briefly:".to_owned(),
            "hello".to_owned(),
        ];

        let parsed = parse_arguments(arguments, Cursor::new("")).unwrap();

        assert_eq!(
            parsed.model,
            "hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K"
        );
        assert_eq!(parsed.prompt, "Answer briefly: hello");
    }

    #[test]
    fn reads_a_non_empty_prompt_from_standard_input_when_arguments_omit_it() {
        let parsed = parse_arguments(
            [
                "conversation-ollama-probe".to_owned(),
                "qwen3.6:27b-q8_0".to_owned(),
            ],
            Cursor::new("Answer privately.\n"),
        )
        .unwrap();

        assert_eq!(parsed.model, "qwen3.6:27b-q8_0");
        assert_eq!(parsed.prompt, "Answer privately.");
    }

    #[test]
    fn rejects_missing_model_or_an_empty_standard_input_prompt_with_usage_error() {
        for arguments in [
            vec!["conversation-ollama-probe".to_owned()],
            vec![
                "conversation-ollama-probe".to_owned(),
                "qwen3.6:27b-q8_0".to_owned(),
            ],
        ] {
            let error = parse_arguments(arguments, Cursor::new(" \n")).unwrap_err();

            assert!(error.starts_with("Usage:"));
        }
    }

    #[test]
    fn rejects_model_identifiers_with_control_characters() {
        let error = parse_arguments(
            [
                "conversation-ollama-probe".to_owned(),
                "test\nmodel".to_owned(),
                "hi".to_owned(),
            ],
            Cursor::new(""),
        )
        .unwrap_err();

        assert!(error.contains("control characters"));
    }

    #[test]
    fn uses_non_zero_timeout_defaults_and_validates_overrides() {
        assert_eq!(
            ProbeTimeouts::defaults(),
            ProbeTimeouts {
                first_delta: Duration::from_secs(60),
                idle: Duration::from_secs(30),
                total: Duration::from_secs(120),
            }
        );
        assert_eq!(
            ProbeTimeouts::from_millis(Some("1"), Some("2"), Some("3")).unwrap(),
            ProbeTimeouts {
                first_delta: Duration::from_millis(1),
                idle: Duration::from_millis(2),
                total: Duration::from_millis(3),
            }
        );
        assert!(ProbeTimeouts::from_millis(Some("0"), None, None).is_err());
        assert!(ProbeTimeouts::from_millis(Some("invalid"), None, None).is_err());
        assert!(ProbeTimeouts::from_millis(Some("18446744073709551615"), None, None).is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn prioritizes_total_timeout_when_total_stage_and_delta_are_ready() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(Ok("delta".to_owned())).await.unwrap();

        let outcome = await_next_delta(
            async { receiver.recv().await },
            tokio::time::Instant::now(),
            Duration::ZERO,
            TimeoutStage::FirstDelta,
        )
        .await;

        assert_eq!(outcome, ReceiveOutcome::Timeout(TimeoutStage::Total));
    }

    #[tokio::test(start_paused = true)]
    async fn prioritizes_stage_timeout_over_a_ready_delta_when_total_is_pending() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(Ok("delta".to_owned())).await.unwrap();

        let outcome = await_next_delta(
            async { receiver.recv().await },
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::ZERO,
            TimeoutStage::Idle,
        )
        .await;

        assert_eq!(outcome, ReceiveOutcome::Timeout(TimeoutStage::Idle));
    }

    #[tokio::test(start_paused = true)]
    async fn returns_a_ready_delta_when_both_deadlines_are_pending() {
        let (sender, mut receiver) = mpsc::channel(1);
        sender.send(Ok("delta".to_owned())).await.unwrap();

        let outcome = await_next_delta(
            async { receiver.recv().await },
            tokio::time::Instant::now() + Duration::from_secs(1),
            Duration::from_secs(1),
            TimeoutStage::FirstDelta,
        )
        .await;

        assert_eq!(outcome, ReceiveOutcome::Delta(Some(Ok("delta".to_owned()))));
    }

    #[test]
    fn formats_request_relative_failure_elapsed_time() {
        let report = format_failure_report(
            "test-model",
            ProbeFailure::adapter(AdapterError::new("failed"), Duration::from_millis(7)),
        );

        assert!(report.contains("status=error\nstage=adapter\nelapsed_ms=7\n"));
    }

    #[test]
    fn formats_success_with_policy_and_unavailable_metrics() {
        let report = format_success_report(
            "test-model",
            Duration::from_millis(12),
            Duration::from_millis(34),
            &OllamaChatMetrics::default(),
        );

        assert_eq!(
            report,
            concat!(
                "model=test-model\nstatus=ok\nfirst_delta_ms=12\ntotal_ms=34\n",
                "think=false\ntemperature=0\nseed=42\nnum_predict=128\nnum_ctx=8192\n",
                "ollama_total_duration_ns=unavailable\n",
                "ollama_load_duration_ns=unavailable\n",
                "ollama_prompt_eval_count=unavailable\n",
                "ollama_prompt_eval_duration_ns=unavailable\n",
                "ollama_eval_count=unavailable\n",
                "ollama_eval_duration_ns=unavailable"
            )
        );
    }

    #[test]
    fn rejects_a_completed_stream_without_a_text_delta() {
        let error = require_first_delta(None).unwrap_err();

        assert_eq!(error, "Ollama response completed without a text delta");
    }

    #[tokio::test]
    async fn probe_disables_thinking_in_the_emitted_chat_request() {
        let server = FakeOllamaServer::start().await;

        run_probe(
            ProbeArguments {
                model: "test-model".into(),
                prompt: "hi".into(),
            },
            server.endpoint(),
        )
        .await
        .unwrap();

        let request_body = server.request_body().await;

        assert!(has_top_level_boolean_field(&request_body, "think", false));
        assert!(request_body.contains(r#""temperature":0.0"#));
        assert!(request_body.contains(r#""seed":42"#));
        assert!(request_body.contains(r#""num_predict":128"#));
        assert!(request_body.contains(r#""num_ctx":8192"#));
    }

    struct FakeOllamaServer {
        endpoint: String,
        request_body: Arc<Mutex<Option<String>>>,
    }

    impl FakeOllamaServer {
        async fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let request_body = Arc::new(Mutex::new(None));
            let stored_request_body = request_body.clone();

            tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                capture_request_and_respond(stream, stored_request_body).await;
            });

            Self {
                endpoint,
                request_body,
            }
        }

        fn endpoint(&self) -> &str {
            &self.endpoint
        }

        async fn request_body(&self) -> String {
            self.request_body.lock().await.clone().unwrap()
        }
    }

    async fn capture_request_and_respond(
        mut stream: TcpStream,
        request_body: Arc<Mutex<Option<String>>>,
    ) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);

            if let Some(header_end) = find_header_end(&request) {
                break header_end;
            }
        };
        let content_length = request[..header_end]
            .windows("Content-Length:".len())
            .position(|window| window.eq_ignore_ascii_case(b"content-length:"))
            .map(|start| {
                let value = &request[start + "Content-Length:".len()..header_end];
                std::str::from_utf8(value)
                    .unwrap()
                    .trim()
                    .parse::<usize>()
                    .unwrap()
            })
            .unwrap();

        while request.len() - header_end < content_length {
            let read = stream.read(&mut buffer).await.unwrap();
            request.extend_from_slice(&buffer[..read]);
        }

        *request_body.lock().await = Some(
            String::from_utf8(request[header_end..header_end + content_length].to_vec()).unwrap(),
        );
        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\n\r\n{\"message\":{\"role\":\"assistant\",\"content\":\"ok\"},\"done\":true}\n",
            )
            .await
            .unwrap();
    }

    fn find_header_end(request: &[u8]) -> Option<usize> {
        request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
    }

    fn has_top_level_boolean_field(document: &str, field: &str, value: bool) -> bool {
        let expected = format!(r#""{field}":{value}"#);
        let mut depth = 0;
        let mut in_string = false;
        let mut escaped = false;

        for (index, character) in document.char_indices() {
            if in_string {
                match character {
                    '\\' if !escaped => escaped = true,
                    '"' if !escaped => in_string = false,
                    _ => escaped = false,
                }
                continue;
            }

            if depth == 1 && document[index..].starts_with(&expected) {
                return true;
            }

            match character {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => depth -= 1,
                _ => {}
            }
        }

        false
    }
}
