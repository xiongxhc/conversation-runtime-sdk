use std::env;
use std::error::Error;
use std::io::{self, Write};
use std::time::Instant;

use conversation_model_adapters::{
    LanguageModel, LanguageModelRequest, OllamaConfig, OllamaLanguageModel,
};
use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Eq, PartialEq)]
struct ProbeArguments {
    model: String,
    prompt: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let arguments = parse_arguments(env::args()).map_err(io::Error::other)?;
    let endpoint = env::var("OLLAMA_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:11434".into());
    run_probe(arguments, &endpoint).await
}

async fn run_probe(arguments: ProbeArguments, endpoint: &str) -> Result<(), Box<dyn Error>> {
    let model = OllamaLanguageModel::new(
        OllamaConfig::new(arguments.model.clone())
            .with_endpoint(endpoint)?
            .with_thinking(false),
    );
    let started_at = Instant::now();
    let mut stream = model.stream(
        LanguageModelRequest::new(TurnId::new(1), arguments.prompt),
        CancellationToken::new(),
    );
    let mut first_delta_at = None;

    while let Some(delta) = stream.recv().await {
        let delta = delta?;
        first_delta_at.get_or_insert_with(Instant::now);
        print!("{delta}");
        io::stdout().flush()?;
    }

    let completed_at = Instant::now();
    let first_delta_ms = require_first_delta(first_delta_at)
        .map_err(io::Error::other)?
        .duration_since(started_at);
    let total_ms = completed_at.duration_since(started_at);

    eprintln!(
        "model={}\nfirst_delta_ms={}\ntotal_ms={}",
        arguments.model,
        first_delta_ms.as_millis(),
        total_ms.as_millis(),
    );

    Ok(())
}

fn parse_arguments(arguments: impl IntoIterator<Item = String>) -> Result<ProbeArguments, String> {
    let mut arguments = arguments.into_iter();
    let _program = arguments.next();
    let Some(model) = arguments.next().filter(|model| !model.trim().is_empty()) else {
        return Err(usage_error());
    };
    let prompt = arguments.collect::<Vec<_>>().join(" ");

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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::{parse_arguments, require_first_delta, run_probe, ProbeArguments};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::Mutex;

    #[test]
    fn parses_exact_model_identifier_and_remaining_prompt_words() {
        let arguments = vec![
            "conversation-ollama-probe".to_owned(),
            "hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K".to_owned(),
            "Answer".to_owned(),
            "briefly:".to_owned(),
            "hello".to_owned(),
        ];

        let parsed = parse_arguments(arguments).unwrap();

        assert_eq!(
            parsed.model,
            "hf.co/mradermacher/Qwen3.6-35B-A3B-abliterated-GGUF:Q6_K"
        );
        assert_eq!(parsed.prompt, "Answer briefly: hello");
    }

    #[test]
    fn rejects_missing_model_or_prompt_with_usage_error() {
        for arguments in [
            vec!["conversation-ollama-probe".to_owned()],
            vec![
                "conversation-ollama-probe".to_owned(),
                "qwen3.6:27b-q8_0".to_owned(),
            ],
        ] {
            let error = parse_arguments(arguments).unwrap_err();

            assert!(error.starts_with("Usage:"));
        }
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
