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
    let model = OllamaLanguageModel::new(
        OllamaConfig::new(arguments.model.clone()).with_endpoint(endpoint)?,
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
    let first_delta_ms = first_delta_at
        .unwrap_or(completed_at)
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

#[cfg(test)]
mod tests {
    use super::parse_arguments;

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
}
