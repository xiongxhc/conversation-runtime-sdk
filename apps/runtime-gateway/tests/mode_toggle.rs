//! `mode = "local" | "api"`: one file holds a local and an API setup per part, one line picks.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use conversation_protocol::RuntimeStatus;
use conversation_runtime_gateway::GatewayConfig;

/// Deliberately fake; nothing in the repository may hold a real credential.
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_TOGGLE_CREDENTIAL";
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const RECORDING_KEY_ENV: &str = "CONVERSATION_TEST_TOGGLE_RECORDING_KEY";

const MEDIA: &str = r#"
[media]
bind = "127.0.0.1:0"
advertised_endpoint = "tcp://127.0.0.1:0"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
"#;

struct Fixture {
    directory: tempfile::TempDir,
    model_path: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
        std::env::set_var(RECORDING_KEY_ENV, "0".repeat(64));
        let directory = tempfile::tempdir().unwrap();
        let model_path = directory.path().join("asr-model");
        std::fs::create_dir(&model_path).unwrap();
        // Building adapters checks that each sidecar exists; nothing here ever runs them.
        for sidecar in ["voice-sidecar", "diarization-sidecar"] {
            let path = directory.path().join(sidecar);
            std::fs::write(&path, "#!/bin/sh\nexit 1\n").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        Self {
            directory,
            model_path,
        }
    }

    /// `header` goes above the first table (`mode`, `mode_overrides`, `profile`, ...).
    fn config(&self, header: &str, media: bool) -> String {
        let root = self.directory.path().display();
        let model_path = self.model_path.display();
        let media = if media { MEDIA } else { "" };
        format!(
            r#"schema_version = 3
{header}

[[provider_hosts]]
id = "language-host"
ownership = "external"
readiness_url = "http://127.0.0.1:11434/api/tags"
startup_timeout_ms = 5000
environment = "inherit"

[[provider_hosts]]
id = "speech-host"
ownership = "external"
readiness_url = "http://127.0.0.1:8000/health"
startup_timeout_ms = 5000
environment = "inherit"

[language.local]
backend = "ollama-compatible"
execution = "local"
provider = "local-language"
provider_host = "language-host"
endpoint = "http://127.0.0.1:11434"
model = "local-model-id"
thinking = false
temperature = 0.7
seed = 42
num_predict = 1024
num_ctx = 8192
max_assistant_content_bytes = 65536

[language.api]
backend = "openai-compatible"
execution = "remote"
provider = "openai"
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-text-model"
credential_env = "{CREDENTIAL_ENV}"
thinking = false
temperature = 0.7
seed = 42
num_predict = 1024
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

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr.local]
backend = "whisperkit"
execution = "local"
provider = "local-speech-recognition"
model_path = "{model_path}"
download = false

[voice.asr.api]
backend = "openai-streaming"
execution = "remote"
provider = "openai-streaming"
endpoint = "wss://api.example.invalid/v1/realtime?intent=transcription"
model = "operator-supplied-transcription-model"
credential_env = "{CREDENTIAL_ENV}"

[voice.asr.api.server_vad]
threshold = 0.5
prefix_padding_ms = 300
silence_duration_ms = 500

[voice.speech.local]
backend = "openai-compatible"
execution = "local"
provider = "local-speech-synthesis"
provider_host = "speech-host"
mode = "streaming"
streaming_interval = 0.2
endpoint = "http://127.0.0.1:8000/v1"
model = "local-speech-model"
voice = "local-voice"
speed = 1.0
language = "auto"
instructions = "Speak naturally and clearly."
max_tokens = 128
repetition_penalty = 1.05
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.speech.api]
backend = "openai-compatible"
execution = "remote"
provider = "openai-compatible-streaming"
mode = "streaming"
streaming_interval = 0.2
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-tts-model"
voice = "operator-supplied-voice"
credential_env = "{CREDENTIAL_ENV}"
speed = 1.0
language = "auto"
instructions = "Speak naturally and clearly."
max_tokens = 128
repetition_penalty = 1.05
max_text_bytes = 4096
max_audio_bytes = 8388608

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "local-audio"
sidecar_executable = "{root}/voice-sidecar"
max_error_bytes = 65536

[voice.diarization.local]
backend = "speechbrain-sidecar"
execution = "local"
provider = "speechbrain"
sidecar_executable = "{root}/diarization-sidecar"
model = "speechbrain/spkrec-ecapa-voxceleb"
model_cache_dir = "{root}/diarization-model"
max_speakers = 12

[voice.diarization.api]
backend = "openai"
execution = "remote"
provider = "openai"
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-diarization-model"
credential_env = "{CREDENTIAL_ENV}"

[recording]
directory = "{root}/recordings"
key_env = "{RECORDING_KEY_ENV}"
{media}"#
        )
    }

    fn write(&self, contents: &str) -> PathBuf {
        let path = self.directory.path().join("gateway.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn validate(&self, header: &str, media: bool) -> Result<RuntimeStatus, String> {
        GatewayConfig::validate(&self.write(&self.config(header, media)))
            .map_err(|error| error.to_string())
    }
}

fn location(status: &RuntimeStatus, kind: &str) -> String {
    status
        .components
        .iter()
        .find(|component| component.kind == kind)
        .unwrap_or_else(|| panic!("status carries no {kind} component"))
        .execution_location
        .clone()
}

fn locations(status: &RuntimeStatus) -> [String; 4] {
    [
        "language_model",
        "speech_recognition",
        "speech_synthesis",
        "diarization",
    ]
    .map(|kind| location(status, kind))
}

#[test]
fn local_mode_selects_every_local_block_and_derives_offline() {
    let fixture = Fixture::new();
    let path = fixture.write(&fixture.config(r#"mode = "local""#, false));

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(adapters.status.profile, "offline");
    assert_eq!(adapters.status.privacy_mode, "local_only");
    assert_eq!(adapters.status.language_location, "local");
    assert_eq!(locations(&adapters.status), ["local"; 4].map(String::from));
    assert!(adapters.media.is_none());
    let hosts: Vec<_> = adapters.provider_hosts().iter().map(|h| h.id()).collect();
    assert_eq!(hosts, ["language-host", "speech-host"]);
}

#[test]
fn api_mode_selects_every_api_block_and_derives_online() {
    let fixture = Fixture::new();
    let path = fixture.write(&fixture.config(r#"mode = "api""#, true));

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(adapters.status.profile, "online");
    assert_eq!(adapters.status.privacy_mode, "cloud");
    assert_eq!(adapters.status.language_location, "remote");
    assert_eq!(locations(&adapters.status), ["remote"; 4].map(String::from));
    assert_eq!(location(&adapters.status, "audio_io"), "local");
    assert!(adapters.media.is_some());
    // Hosts that only the unselected local blocks use are not started.
    assert!(adapters.provider_hosts().is_empty());
}

#[test]
fn an_override_moves_one_part_to_the_other_side() {
    let fixture = Fixture::new();
    let path = fixture.write(&fixture.config(
        "mode = \"api\"\nmode_overrides = { speech = \"local\", diarization = \"local\" }",
        true,
    ));

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(adapters.status.profile, "online");
    assert_eq!(adapters.status.privacy_mode, "cloud");
    assert_eq!(
        locations(&adapters.status),
        ["remote", "remote", "local", "local"].map(String::from)
    );
    let hosts: Vec<_> = adapters.provider_hosts().iter().map(|h| h.id()).collect();
    assert_eq!(hosts, ["speech-host"]);
}

#[test]
fn one_api_part_in_a_local_file_makes_the_deployment_online() {
    let fixture = Fixture::new();

    let status = fixture
        .validate(
            "mode = \"local\"\nmode_overrides = { language = \"api\" }",
            false,
        )
        .unwrap();

    assert_eq!(status.profile, "online");
    assert_eq!(status.privacy_mode, "cloud");
    assert_eq!(
        locations(&status),
        ["remote", "local", "local", "local"].map(String::from)
    );
}

#[test]
fn local_asr_listens_through_the_sidecar_so_the_media_bridge_is_dropped() {
    let fixture = Fixture::new();

    for header in [
        "mode = \"api\"\nmode_overrides = { asr = \"local\" }",
        "mode = \"local\"",
    ] {
        let path = fixture.write(&fixture.config(header, true));
        let adapters = GatewayConfig::load(&path).unwrap();
        assert_eq!(location(&adapters.status, "speech_recognition"), "local");
        assert!(adapters.media.is_none(), "{header}");
        assert!(!adapters
            .status
            .capabilities
            .iter()
            .any(|capability| capability == "media_lease_renewal"));
    }
    let status = fixture
        .validate("mode = \"api\"\nmode_overrides = { asr = \"local\" }", true)
        .unwrap();
    assert_eq!(status.profile, "online");
}

#[test]
fn api_asr_still_requires_a_media_bridge() {
    let fixture = Fixture::new();

    let error = fixture.validate(r#"mode = "api""#, false).unwrap_err();

    assert_eq!(error, "online voice requires a media bridge configuration");
}

#[test]
fn an_explicit_profile_or_privacy_mode_must_agree_with_the_selection() {
    let fixture = Fixture::new();

    for header in [
        "mode = \"api\"\nprofile = \"online\"\nprivacy_mode = \"cloud\"",
        "mode = \"local\"\nprofile = \"offline\"\nprivacy_mode = \"local-only\"",
    ] {
        let media = header.contains("api");
        fixture.validate(header, media).unwrap();
    }
    for (header, media) in [
        ("mode = \"api\"\nprofile = \"offline\"", true),
        ("mode = \"api\"\nprivacy_mode = \"local-only\"", true),
        ("mode = \"local\"\nprofile = \"online\"", false),
        ("mode = \"local\"\nprivacy_mode = \"cloud\"", false),
    ] {
        let error = fixture.validate(header, media).unwrap_err();
        assert!(
            error.contains("profile") || error.contains("privacy_mode"),
            "{header}: {error}"
        );
        assert!(error.contains("mode"), "{header}: {error}");
    }
}

#[test]
fn the_selected_block_must_exist() {
    let fixture = Fixture::new();
    let without_speech_api = fixture.config(r#"mode = "api""#, true).replacen(
        "[voice.speech.api]",
        "[voice.speech.unused]",
        1,
    );

    let error = GatewayConfig::validate(&fixture.write(&without_speech_api))
        .unwrap_err()
        .to_string();

    assert!(error.contains("[voice.speech.api]"), "{error}");
}

#[test]
fn a_selected_block_that_declares_the_other_side_is_refused() {
    let fixture = Fixture::new();
    let swapped = fixture
        .config(r#"mode = "local""#, false)
        .replacen("[language.local]", "[language.spare]", 1)
        .replacen("[language.api]", "[language.local]", 1)
        .replacen("[language.spare]", "[language.api]", 1);

    let error = GatewayConfig::validate(&fixture.write(&swapped))
        .unwrap_err()
        .to_string();

    assert_eq!(error, "language execution must be local");
}

#[test]
fn malformed_switches_fail_with_a_named_reason() {
    let fixture = Fixture::new();

    for (header, needle) in [
        (r#"mode = "cloud""#, "mode"),
        (
            "mode = \"api\"\nmode_overrides = { tts = \"local\" }",
            "mode_overrides",
        ),
        (
            "mode = \"api\"\nmode_overrides = { speech = \"remote\" }",
            "mode_overrides",
        ),
        ("mode_overrides = { speech = \"local\" }", "mode"),
    ] {
        let error = fixture.validate(header, true).unwrap_err();
        assert!(error.contains(needle), "{header}: {error}");
    }

    // A part written the old way cannot hide inside a file that sets `mode`.
    let plain = fixture
        .config(r#"mode = "local""#, false)
        .replacen("[language.local]", "[language]", 1)
        .replacen("[language.api]", "[unused_language]", 1);
    let error = GatewayConfig::validate(&fixture.write(&plain))
        .unwrap_err()
        .to_string();
    assert!(error.contains("[language.local]"), "{error}");

    // `mode` needs the schema that can state a profile at all.
    let legacy = fixture.config(r#"mode = "local""#, false).replacen(
        "schema_version = 3",
        "schema_version = 2",
        1,
    );
    let error = GatewayConfig::validate(&fixture.write(&legacy))
        .unwrap_err()
        .to_string();
    assert!(error.contains("schema_version 3"), "{error}");
}

#[test]
fn an_override_for_an_unconfigured_part_is_refused() {
    let fixture = Fixture::new();
    let without_diarization = fixture
        .config(
            "mode = \"api\"\nmode_overrides = { diarization = \"local\" }",
            true,
        )
        .replacen("[voice.diarization.local]", "[unused_local]", 1)
        .replacen("[voice.diarization.api]", "[unused_api]", 1);

    let error = GatewayConfig::validate(&fixture.write(&without_diarization))
        .unwrap_err()
        .to_string();

    assert!(error.contains("diarization"), "{error}");
}

/// Every published gateway example loads with the profile it documents; the toggle example
/// loads in both positions of its switch.
#[test]
fn every_published_gateway_example_still_loads() {
    let fixture = Fixture::new();
    std::env::set_var("OPENAI_API_KEY", CREDENTIAL_VALUE);
    std::env::set_var("MEETING_RECORDING_KEY", "0".repeat(64));
    let configs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs");
    let mut examples: Vec<_> = std::fs::read_dir(&configs)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("gateway") && name.ends_with(".toml"))
        })
        .collect();
    examples.sort();
    let names: Vec<_> = examples
        .iter()
        .map(|path| path.file_name().unwrap().to_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        [
            "gateway.example.toml",
            "gateway.meeting-online.example.toml",
            "gateway.toggle.example.toml",
        ]
    );

    let expected = ["offline", "online", "online"];
    for (path, profile) in examples.iter().zip(expected) {
        let status = GatewayConfig::validate(&path.canonicalize().unwrap())
            .unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_eq!(status.profile, profile, "{}", path.display());
    }

    // Flip the toggle example to local. Its local ASR model directory has to exist on this
    // machine, so the placeholder path is pointed at the fixture's directory.
    let toggle = std::fs::read_to_string(configs.join("gateway.toggle.example.toml")).unwrap();
    assert!(toggle.contains("\nmode = \"api\"\n"));
    let local = toggle
        .replacen("\nmode = \"api\"\n", "\nmode = \"local\"\n", 1)
        .replace(
            "/absolute/path/to/local-asr-model",
            &fixture.model_path.display().to_string(),
        );
    let status = GatewayConfig::validate(&fixture.write(&local)).unwrap();
    assert_eq!(status.profile, "offline");
    assert_eq!(status.privacy_mode, "local_only");
}
