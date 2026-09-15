//! Regression coverage for the explicit online meeting profile. The configuration names remote
//! providers, but all requests remain unstarted here: this catches the previous construction
//! branch that rejected online configurations before any gateway session could use them.

use conversation_runtime_gateway::GatewayConfig;

const CREDENTIAL_ENV: &str = "CONVERSATION_TASK7_TEST_CREDENTIAL";
const CREDENTIAL: &str = "fake-not-a-real-credential-0000";

/// Catches the former `GatewayConfig::build_adapters` branch that rejected every online
/// deployment (or replaced it with an offline adapter) instead of constructing the selected
/// remote adapter set and reporting that selection truthfully.
#[test]
fn online_profile_constructs_remote_adapters_and_text_only_status_never_advertises_voice() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("meeting-online.toml");
    std::fs::write(&path, online_config()).unwrap();

    let adapters = GatewayConfig::load(&path).expect("the explicit online adapter set constructs");

    assert_eq!(adapters.status.profile, "online");
    assert_eq!(adapters.status.privacy_mode, "cloud");
    assert!(adapters.voice.is_some());
    assert!(adapters.status.components.iter().any(|component| {
        component.kind == "language_model" && component.execution_location == "remote"
    }));
    assert!(adapters.status.components.iter().any(|component| {
        component.kind == "speech_recognition" && component.execution_location == "remote"
    }));
    assert!(adapters.status.components.iter().any(|component| {
        component.kind == "speech_synthesis" && component.execution_location == "remote"
    }));

    let text_only = adapters.text_only_status();
    assert!(!text_only
        .capabilities
        .iter()
        .any(|value| value == "voice_session"));
    assert!(text_only.components.iter().all(|component| {
        !matches!(
            component.kind.as_str(),
            "speech_recognition" | "speech_synthesis" | "audio_io"
        )
    }));
}

fn online_config() -> String {
    format!(
        r#"schema_version = 3
profile = "online"
privacy_mode = "cloud"

[language]
backend = "openai-compatible"
execution = "remote"
provider = "scripted-online-language"
endpoint = "https://provider.example.invalid/v1"
model = "test-text-model"
credential_env = "{CREDENTIAL_ENV}"
thinking = false
temperature = 0.7
seed = 42
num_predict = 128
num_ctx = 8192
max_assistant_content_bytes = 65536

[persona]
mode = "direct-answer"
warmth = 80
humor = 60
teasing = 40
initiative = 35
directness = 80
intimacy = 30
verbosity = 20
follow_up_frequency = 25

[voice]
response_mode = "external_gate"

[media]
bind = "127.0.0.1:0"
advertised_endpoint = "tcp://127.0.0.1:0"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "openai-streaming"
execution = "remote"
provider = "scripted-online-asr"
endpoint = "wss://provider.example.invalid/v1/realtime?intent=transcription"
model = "test-asr-model"
credential_env = "{CREDENTIAL_ENV}"

[voice.asr.server_vad]
threshold = 0.5
prefix_padding_ms = 300
silence_duration_ms = 500

[voice.speech]
backend = "openai-compatible"
execution = "remote"
provider = "scripted-online-speech"
mode = "streaming"
streaming_interval = 0.2
endpoint = "https://provider.example.invalid/v1"
model = "test-speech-model"
voice = "test-voice"
credential_env = "{CREDENTIAL_ENV}"
speed = 1.0
language = "auto"
instructions = "Speak clearly."
max_tokens = 128
repetition_penalty = 1.0
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "scripted-local-audio"
sidecar_executable = "/absolute/path/to/unused-sidecar"
max_error_bytes = 65536
"#
    )
}
