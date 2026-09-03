//! The local diarization backend: a sidecar process behind the same batch diarization port the
//! online backend sits behind.
//!
//! Every test here drives a **fake sidecar** — a small shell script this file writes into a
//! `tempfile` directory — rather than the real Python one. The real sidecar needs a downloaded
//! model and the torch stack, which is a human-gated install; what the runtime owes the
//! deployment is the process contract, and that is what a fake can hold it to.
//!
//! Nothing here reaches the network, reads a real key, or writes outside a temporary directory.
//! The recording key is a fixed fake value on a variable each test owns.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use conversation_model_adapters::{
    AudioFrame, PcmFormat, PcmSampleFormat, SpeechbrainDiarizationConfig,
    SpeechbrainTranscriptionDiarizer, TranscriptionDiarizer,
};
use conversation_protocol::{GenerationId, SessionId, TurnId, UtteranceId};
use conversation_runtime::{
    reconcile_recording, RecordingSettings, RecordingSummary, SessionRecorder,
};
use conversation_runtime_gateway::GatewayConfig;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

/// A fake 256-bit key, hexadecimal as the recording sink requires. Not a real key, and used
/// only against a temporary directory this test owns.
const FAKE_KEY: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
/// The one credential value in this file, for the online configuration cases.
const FAKE_CREDENTIAL: &str = "fake-not-a-real-credential-0000";
/// Every meeting the gateway records runs under this identifier, which is why the handle is
/// what isolates one recording from another.
const GATEWAY_SESSION: SessionId = SessionId::new(1);
/// One second of 16 kHz mono signed 16-bit audio.
const ONE_SECOND_BYTES: usize = 32_000;
/// A byte pattern encryption cannot leave intact. Every plaintext assertion searches for it.
const PLAINTEXT_MARKER: &[u8] = b"PLAINTEXT-MEETING-AUDIO-MARKER-32";

const THREE_SEGMENTS: &str = r#"{"type":"result","segments":[{"start_ms":0,"end_ms":1000,"speaker":"spk-1"},{"start_ms":1000,"end_ms":2000,"speaker":null},{"start_ms":2000,"end_ms":3000,"speaker":"spk-2"}],"warnings":[]}"#;

#[tokio::test]
async fn a_local_sidecar_answers_a_recording_with_anonymous_clusters_and_is_reaped() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_OK");
    let sidecar = fixture.sidecar(Script::answering(THREE_SEGMENTS));
    let summary = fixture.record(3);

    let result = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect("the sidecar answered");

    let segments = result.segments();
    assert_eq!(segments.len(), 3);
    assert_eq!(
        segments
            .iter()
            .map(|segment| {
                (
                    segment.segment_id().to_owned(),
                    segment.started_at_ms(),
                    segment.ended_at_ms(),
                    segment.text().to_owned(),
                    segment.speaker_cluster().map(str::to_owned),
                )
            })
            .collect::<Vec<_>>(),
        vec![
            (
                "seg-1".to_owned(),
                0,
                1_000,
                String::new(),
                Some("cluster-1".to_owned())
            ),
            ("seg-2".to_owned(), 1_000, 2_000, String::new(), None),
            (
                "seg-3".to_owned(),
                2_000,
                3_000,
                String::new(),
                Some("cluster-2".to_owned())
            ),
        ],
        "the local backend diarizes and does not claim to have heard words"
    );
    assert!(result.warnings().is_empty(), "{:?}", result.warnings());

    // What the sidecar was told: its own configuration, and no path, handle, or key.
    let start = sidecar.start_line();
    assert!(
        start.contains(r#""type":"start""#)
            && start.contains(r#""protocol_version":1"#)
            && start.contains(r#""model":"speechbrain/spkrec-ecapa-voxceleb""#)
            && start.contains(r#""max_speakers":12"#)
            && start.contains(r#""encoding":"pcm_s16le""#)
            && start.contains(r#""sample_rate_hz":16000"#)
            && start.contains(r#""channels":1"#),
        "{start}"
    );
    assert!(
        !start.contains(&summary.recording_handle)
            && !start.contains(FAKE_KEY)
            && !start.contains(fixture.key_env)
            && !start.contains(&summary.encrypted_path.display().to_string()),
        "the sidecar is told nothing about where the recording lives: {start}"
    );
    // One window of three seconds, delivered as bytes on the pipe.
    assert_eq!(sidecar.request_kinds(), vec!["audio".to_owned()]);
    assert!(sidecar.exited(), "the sidecar process was reaped");
}

#[tokio::test]
async fn a_sidecar_that_never_answers_is_stopped_and_reaped_at_the_timeout() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_TIMEOUT");
    let sidecar = fixture.sidecar(Script::on_finish("sleep 60"));
    let summary = fixture.record(1);

    let error = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect_err("a sidecar that never answers cannot produce a result");

    assert_eq!(error.message(), "local diarization sidecar stalled");
    assert!(
        sidecar.exited(),
        "the stalled sidecar was killed and reaped"
    );
}

#[tokio::test]
async fn a_sidecar_line_the_runtime_cannot_read_fails_closed_without_quoting_it() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_MALFORMED");
    let sidecar = fixture.sidecar(Script::on_finish(
        "printf '%s\\n' 'not json at all: /Users/cx/private/meeting.wav'",
    ));
    let summary = fixture.record(1);

    let error = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect_err("an unreadable line is not an answer");

    assert_eq!(
        error.message(),
        "local diarization sidecar returned an unsupported answer"
    );
    assert!(
        !error.message().contains("meeting.wav"),
        "a diagnostic never quotes the sidecar's own words"
    );
    assert!(sidecar.exited());
}

#[tokio::test]
async fn a_sidecar_that_exits_before_answering_states_its_exit_rather_than_a_result() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_EXIT");
    let sidecar = fixture.sidecar(Script::exiting_after_start(3));
    let summary = fixture.record(1);

    let error = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect_err("a sidecar that exits produces no result");

    assert_eq!(
        error.message(),
        "local diarization sidecar exited without answering"
    );
    assert!(sidecar.exited());
}

#[tokio::test]
async fn a_sidecar_failure_is_stated_as_one_code_from_the_closed_list() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_FAILURE");
    let sidecar = fixture.sidecar(Script::on_finish(
        r#"printf '%s\n' '{"type":"failure","code":"model_unavailable"}'"#,
    ));
    let summary = fixture.record(1);

    let error = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect_err("a failure is not a result");
    assert_eq!(
        error.message(),
        "local diarization sidecar failed: model_unavailable"
    );

    // A code this runtime does not know is refused rather than relayed, so the diagnostic
    // vocabulary cannot be widened from the far side of the pipe.
    let invented = fixture.sidecar(Script::on_finish(
        r#"printf '%s\n' '{"type":"failure","code":"chris_was_not_recognised"}'"#,
    ));
    let error = fixture
        .reconcile(
            &summary.recording_handle,
            &invented,
            CancellationToken::new(),
        )
        .await
        .expect_err("an undeclared code is not an answer");
    assert_eq!(
        error.message(),
        "local diarization sidecar returned an unsupported answer"
    );
}

#[tokio::test]
async fn a_speaker_label_the_sidecar_invents_never_reaches_the_result() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_IDENTITY");
    let sidecar = fixture.sidecar(Script::answering(
        r#"{"type":"result","segments":[{"start_ms":0,"end_ms":500,"speaker":"Chris Xiong"},{"start_ms":500,"end_ms":1000,"speaker":"Chris Xiong"}],"warnings":["low_confidence"]}"#,
    ));
    let summary = fixture.record(1);

    let result = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect("the answer is usable, its labels are not");

    assert_eq!(
        result
            .segments()
            .iter()
            .map(|segment| segment.speaker_cluster())
            .collect::<Vec<_>>(),
        vec![Some("cluster-1"), Some("cluster-1")],
        "one voice stays one cluster, and the name it was given is discarded"
    );
    assert!(
        !format!("{result:?}").contains("Chris Xiong"),
        "the rendered result carries no name: {result:?}"
    );
    assert_eq!(
        result
            .warnings()
            .iter()
            .map(|warning| warning.as_str())
            .collect::<Vec<_>>(),
        vec!["provider_reported_warnings"],
        "the sidecar's warnings are counted, not relayed"
    );
}

#[tokio::test]
async fn no_plaintext_audio_reaches_disk_on_success_failure_or_cancellation() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_PLAINTEXT");
    let summary = fixture.record(4);
    let before_temp = walk(&std::env::temp_dir());
    let working = std::env::current_dir().expect("a working directory");
    let before_working = walk(&working);

    let succeeding = fixture.sidecar(Script::answering(THREE_SEGMENTS));
    assert!(fixture
        .reconcile(
            &summary.recording_handle,
            &succeeding,
            CancellationToken::new()
        )
        .await
        .is_ok());

    let failing = fixture.sidecar(Script::on_finish(
        r#"printf '%s\n' '{"type":"failure","code":"internal_error"}'"#,
    ));
    assert!(fixture
        .reconcile(
            &summary.recording_handle,
            &failing,
            CancellationToken::new()
        )
        .await
        .is_err());

    let cancelling = fixture.sidecar(Script::on_finish("sleep 60"));
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        token.cancel();
    });
    assert!(fixture
        .reconcile(&summary.recording_handle, &cancelling, cancellation)
        .await
        .is_err());

    // Every file under the fixture — which holds the recording, the sidecars, and everything
    // they wrote — plus every file that appeared under the process temporary directory and the
    // working directory while the three reconciliations ran.
    let mut scanned = 0_usize;
    let mut paths = walk(fixture.directory.path());
    paths.extend(
        walk(&std::env::temp_dir())
            .into_iter()
            .filter(|path| !before_temp.contains(path)),
    );
    paths.extend(
        walk(&working)
            .into_iter()
            .filter(|path| !before_working.contains(path)),
    );
    for path in paths {
        scanned += 1;
        assert!(
            !std::fs::read(&path)
                .unwrap_or_default()
                .windows(PLAINTEXT_MARKER.len())
                .any(|window| window == PLAINTEXT_MARKER),
            "plaintext audio reached {}",
            path.display()
        );
    }
    assert!(scanned > 0, "the scan must actually have read something");
}

#[tokio::test]
async fn a_recording_another_session_made_never_starts_a_sidecar() {
    let fixture = Fixture::new("CONVERSATION_TEST_LOCAL_DIARIZATION_HANDLE");
    let sidecar = fixture.sidecar(Script::answering(THREE_SEGMENTS));
    let summary = fixture.record_as(SessionId::new(9), 1);

    let error = fixture
        .reconcile(
            &summary.recording_handle,
            &sidecar,
            CancellationToken::new(),
        )
        .await
        .expect_err("a recording answers only to the session that made it");
    assert_eq!(error.message(), "recording was not made by that session");

    for handle in ["../escape", "", "0123456789ABCDEF0123456789abcdef", "zz"] {
        assert!(
            fixture
                .reconcile(handle, &sidecar, CancellationToken::new())
                .await
                .is_err(),
            "accepted {handle}"
        );
    }
    assert!(
        !sidecar.pid_path.exists(),
        "a handle the runtime refuses never spawns a process"
    );
}

#[test]
fn the_offline_profile_refuses_the_online_diarization_backend() {
    let fixture = ConfigFixture::new();

    let status = GatewayConfig::validate(&fixture.write(&fixture.offline(SPEECHBRAIN_SECTION)))
        .expect("an offline deployment may diarize locally");
    assert_eq!(status.profile, "offline");

    let error = GatewayConfig::validate(&fixture.write(&fixture.offline(OPENAI_SECTION)))
        .expect_err("an offline deployment must not send its meeting to a provider");
    assert!(
        error
            .to_string()
            .contains("offline gateway diarization backend must run locally"),
        "{error}"
    );
    assert!(
        !error.to_string().contains(FAKE_CREDENTIAL),
        "a diagnostic never carries a credential value"
    );
}

#[test]
fn an_online_profile_accepts_either_diarization_backend() {
    let fixture = ConfigFixture::new();

    for section in [SPEECHBRAIN_SECTION, OPENAI_SECTION] {
        let status = GatewayConfig::validate(&fixture.write(&fixture.online(section)))
            .expect("an online meeting chooses its diarization backend");
        assert_eq!(status.profile, "online");
    }

    // The backend and the execution it declares must agree in either profile.
    for section in [
        SPEECHBRAIN_SECTION.replace("execution = \"local\"", "execution = \"remote\""),
        OPENAI_SECTION.replace("execution = \"remote\"", "execution = \"local\""),
    ] {
        assert!(
            GatewayConfig::validate(&fixture.write(&fixture.online(&section))).is_err(),
            "accepted {section}"
        );
    }
}

#[test]
fn a_local_diarization_deployment_lists_the_component_as_local() {
    let fixture = ConfigFixture::new();

    let online = GatewayConfig::validate(&fixture.write(&fixture.online(SPEECHBRAIN_SECTION)))
        .expect("valid online configuration");
    let component = online
        .components
        .iter()
        .find(|component| component.kind == "diarization")
        .expect("the status names the diarization component");
    assert_eq!(component.execution_location, "local");
    assert_eq!(component.provider_label, "speechbrain");
    assert!(
        online
            .components
            .iter()
            .any(|component| component.execution_location == "remote"),
        "an online deployment still names what it moved off the device"
    );

    let remote = GatewayConfig::validate(&fixture.write(&fixture.online(OPENAI_SECTION)))
        .expect("valid online configuration");
    assert_eq!(
        remote
            .components
            .iter()
            .find(|component| component.kind == "diarization")
            .map(|component| component.execution_location.as_str()),
        Some("remote")
    );

    let offline = GatewayConfig::validate(&fixture.write(&fixture.offline(SPEECHBRAIN_SECTION)))
        .expect("valid offline configuration");
    assert!(
        offline
            .components
            .iter()
            .all(|component| component.execution_location == "local"),
        "an offline deployment keeps every component local"
    );
}

#[test]
fn a_diarization_section_requires_schema_three_and_bounded_values() {
    let fixture = ConfigFixture::new();

    let legacy = fixture
        .offline(SPEECHBRAIN_SECTION)
        .replace("schema_version = 3", "schema_version = 2")
        .replace("profile = \"offline\"\n", "");
    assert!(
        GatewayConfig::validate(&fixture.write(&legacy)).is_err(),
        "diarization requires schema version 3"
    );

    let refused = [
        SPEECHBRAIN_SECTION.replace(
            "backend = \"speechbrain-sidecar\"",
            "backend = \"pyannote\"",
        ),
        SPEECHBRAIN_SECTION.replace("max_speakers = 12", "max_speakers = 1"),
        SPEECHBRAIN_SECTION.replace("max_speakers = 12", "max_speakers = 65"),
        SPEECHBRAIN_SECTION.replace("provider = \"speechbrain\"", "provider = \"\""),
        SPEECHBRAIN_SECTION.replace(
            "model = \"speechbrain/spkrec-ecapa-voxceleb\"",
            "model = \"\"",
        ),
        SPEECHBRAIN_SECTION.replace("/absolute/path/to/diarization-sidecar", "relative/sidecar"),
        SPEECHBRAIN_SECTION.replace("/absolute/path/to/models", "/models/../escape"),
        SPEECHBRAIN_SECTION.replace("/absolute/path/to/models", "models"),
        format!("{SPEECHBRAIN_SECTION}credential_env = \"CONVERSATION_TEST_DIARIZATION_KEY\"\n"),
        format!("{SPEECHBRAIN_SECTION}endpoint = \"https://api.example.invalid/v1\"\n"),
        format!("{SPEECHBRAIN_SECTION}unknown_key = \"value\"\n"),
        OPENAI_SECTION.replace("endpoint = \"https://api.example.invalid/v1\"", ""),
        OPENAI_SECTION.replace(
            "endpoint = \"https://api.example.invalid/v1\"",
            "endpoint = \"http://127.0.0.1:9000/v1\"",
        ),
        OPENAI_SECTION.replace(
            &format!("credential_env = \"{DIARIZATION_CREDENTIAL_ENV}\""),
            "",
        ),
        format!("{OPENAI_SECTION}max_speakers = 12\n"),
        format!("{OPENAI_SECTION}sidecar_executable = \"/absolute/path/to/x\"\n"),
    ];
    for section in refused {
        assert!(
            GatewayConfig::validate(&fixture.write(&fixture.online(&section))).is_err(),
            "accepted {section}"
        );
    }
}

// --- the configuration fixtures ---------------------------------------------------------------

const DIARIZATION_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_DIARIZATION_CREDENTIAL";
const RECORDING_KEY_ENV: &str = "CONVERSATION_TEST_DIARIZATION_RECORDING_KEY";

const SPEECHBRAIN_SECTION: &str = r#"[voice.diarization]
backend = "speechbrain-sidecar"
execution = "local"
provider = "speechbrain"
sidecar_executable = "/absolute/path/to/diarization-sidecar"
model = "speechbrain/spkrec-ecapa-voxceleb"
model_cache_dir = "/absolute/path/to/models"
max_speakers = 12
"#;

const OPENAI_SECTION: &str = r#"[voice.diarization]
backend = "openai"
execution = "remote"
provider = "openai"
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-diarization-model"
credential_env = "CONVERSATION_TEST_DIARIZATION_CREDENTIAL"
"#;

struct ConfigFixture {
    directory: TempDir,
    asr_model_path: PathBuf,
    voice_sidecar: PathBuf,
}

impl ConfigFixture {
    fn new() -> Self {
        std::env::set_var(DIARIZATION_CREDENTIAL_ENV, FAKE_CREDENTIAL);
        std::env::set_var(RECORDING_KEY_ENV, FAKE_KEY);
        let directory = tempfile::tempdir().unwrap();
        let asr_model_path = directory.path().join("asr-model");
        std::fs::create_dir(&asr_model_path).unwrap();
        let voice_sidecar = directory.path().join("voice-sidecar");
        std::fs::write(&voice_sidecar, "#!/bin/sh\n").unwrap();
        Self {
            directory,
            asr_model_path,
            voice_sidecar,
        }
    }

    fn write(&self, contents: &str) -> PathBuf {
        let path = self.directory.path().join("gateway.toml");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn online(&self, diarization: &str) -> String {
        format!(
            "{}\n{diarization}",
            ONLINE_CONFIG.replace("SIDECAR", &self.voice_sidecar.display().to_string())
        )
    }

    fn offline(&self, diarization: &str) -> String {
        format!(
            "{}\n{diarization}",
            OFFLINE_CONFIG
                .replace("SIDECAR", &self.voice_sidecar.display().to_string())
                .replace("ASR_MODEL", &self.asr_model_path.display().to_string())
        )
    }
}

const ONLINE_CONFIG: &str = r#"schema_version = 3
profile = "online"
privacy_mode = "cloud"

[language]
backend = "openai-compatible"
execution = "remote"
provider = "openai"
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-text-model"
credential_env = "CONVERSATION_TEST_DIARIZATION_CREDENTIAL"
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

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "openai-streaming"
execution = "remote"
provider = "openai-streaming"
model = "operator-supplied-transcription-model"
credential_env = "CONVERSATION_TEST_DIARIZATION_CREDENTIAL"

[voice.speech]
backend = "openai-compatible"
execution = "remote"
provider = "openai-compatible-streaming"
mode = "streaming"
streaming_interval = 0.2
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-tts-model"
voice = "operator-supplied-voice"
credential_env = "CONVERSATION_TEST_DIARIZATION_CREDENTIAL"
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
sidecar_executable = "SIDECAR"
max_error_bytes = 65536
"#;

const OFFLINE_CONFIG: &str = r#"schema_version = 3
profile = "offline"
privacy_mode = "local-only"

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

[language]
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

[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "whisperkit"
execution = "local"
provider = "local-speech-recognition"
model_path = "ASR_MODEL"
download = false

[voice.speech]
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

[voice.audio]
backend = "managed-sidecar"
execution = "local"
provider = "local-audio"
sidecar_executable = "SIDECAR"
max_error_bytes = 65536
"#;

// --- the recording and sidecar fixtures -------------------------------------------------------

struct Fixture {
    directory: TempDir,
    key_env: &'static str,
    settings: RecordingSettings,
    cache: PathBuf,
    spawned: std::cell::Cell<usize>,
}

impl Fixture {
    fn new(key_env: &'static str) -> Self {
        std::env::set_var(key_env, FAKE_KEY);
        let directory = tempfile::tempdir().unwrap();
        let recordings = directory.path().join("recordings");
        std::fs::create_dir(&recordings).unwrap();
        let cache = directory.path().join("model-cache");
        std::fs::create_dir(&cache).unwrap();
        let settings = RecordingSettings::new(&recordings, key_env).unwrap();
        Self {
            directory,
            key_env,
            settings,
            cache,
            spawned: std::cell::Cell::new(0),
        }
    }

    fn record(&self, frames: u64) -> RecordingSummary {
        self.record_as(GATEWAY_SESSION, frames)
    }

    fn record_as(&self, session_id: SessionId, frames: u64) -> RecordingSummary {
        let mut recorder = SessionRecorder::start(&self.settings, session_id).unwrap();
        for sequence in 0..frames {
            recorder.record_frame(&frame(sequence)).unwrap();
        }
        recorder.finalize().unwrap()
    }

    fn sidecar(&self, script: Script) -> FakeSidecar {
        let index = self.spawned.get();
        self.spawned.set(index + 1);
        let root = self.directory.path().join(format!("sidecar-{index}"));
        std::fs::create_dir(&root).unwrap();
        let executable = root.join("diarization-sidecar");
        let start_path = root.join("start-line");
        let log_path = root.join("requests");
        let pid_path = root.join("pid");
        std::fs::write(
            &executable,
            script.render(&start_path, &log_path, &pid_path),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&executable).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).unwrap();
        FakeSidecar {
            executable,
            start_path,
            log_path,
            pid_path,
        }
    }

    async fn reconcile(
        &self,
        handle: &str,
        sidecar: &FakeSidecar,
        cancellation: CancellationToken,
    ) -> Result<conversation_model_adapters::DiarizationResult, conversation_protocol::RuntimeError>
    {
        let config = SpeechbrainDiarizationConfig::new(
            &sidecar.executable,
            "speechbrain/spkrec-ecapa-voxceleb",
            &self.cache,
            12,
        )
        .unwrap()
        .with_startup_timeout(Duration::from_secs(5))
        .unwrap()
        .with_request_timeout(Duration::from_millis(600))
        .unwrap();
        let diarizer: Arc<dyn TranscriptionDiarizer> =
            Arc::new(SpeechbrainTranscriptionDiarizer::new(config));
        reconcile_recording(
            &self.settings,
            GATEWAY_SESSION,
            handle,
            diarizer,
            cancellation,
        )
        .await
    }
}

/// The fake sidecar's behaviour, as the shell fragments that produce it.
struct Script {
    after_start: String,
    on_finish: String,
}

impl Script {
    fn answering(result: &str) -> Self {
        Self {
            after_start: String::new(),
            on_finish: format!("printf '%s\\n' '{result}'"),
        }
    }

    fn on_finish(fragment: &str) -> Self {
        Self {
            after_start: String::new(),
            on_finish: fragment.to_owned(),
        }
    }

    fn exiting_after_start(code: i32) -> Self {
        Self {
            after_start: format!("exit {code}"),
            on_finish: "exit 0".to_owned(),
        }
    }

    /// The sidecar contract in twelve lines of shell: read the start line, announce readiness,
    /// take the audio lines, answer once at `finish`.
    ///
    /// Only the *kind* of each request after the start line is written down, never its content:
    /// an audio line carries the meeting, and a test fixture that logged it would put the very
    /// plaintext this task forbids onto disk.
    fn render(&self, start: &Path, log: &Path, pid: &Path) -> String {
        let start = start.display();
        let log = log.display();
        let pid = pid.display();
        let Self {
            after_start,
            on_finish,
        } = self;
        format!(
            r#"#!/bin/sh
printf '%s' "$$" > '{pid}'
IFS= read -r start_line || exit 4
printf '%s' "$start_line" > '{start}'
{after_start}
printf '%s\n' '{{"type":"ready","protocol_version":1}}'
while IFS= read -r request; do
  case "$request" in
    *'"type":"finish"'*) printf '%s\n' finish >> '{log}'; {on_finish}; exit 0;;
    *'"type":"audio"'*) printf '%s\n' audio >> '{log}';;
    *) printf '%s\n' other >> '{log}';;
  esac
done
exit 0
"#
        )
    }
}

struct FakeSidecar {
    executable: PathBuf,
    start_path: PathBuf,
    log_path: PathBuf,
    pid_path: PathBuf,
}

impl FakeSidecar {
    fn start_line(&self) -> String {
        std::fs::read_to_string(&self.start_path).expect("the sidecar was given a start line")
    }

    /// Every request kind the sidecar saw after the start line, except the closing `finish`.
    fn request_kinds(&self) -> Vec<String> {
        std::fs::read_to_string(&self.log_path)
            .unwrap_or_default()
            .lines()
            .filter(|kind| *kind != "finish")
            .map(str::to_owned)
            .collect()
    }

    /// Whether the process is gone: `kill -0` fails for a reaped process and succeeds for a
    /// live one *and* for a zombie, so this is a statement about reaping, not just exiting.
    fn exited(&self) -> bool {
        let Ok(pid) = std::fs::read_to_string(&self.pid_path) else {
            return false;
        };
        !std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("kill runs")
            .success()
    }
}

fn frame(sequence: u64) -> AudioFrame {
    let mut payload = vec![0x11_u8; ONE_SECOND_BYTES];
    payload[..PLAINTEXT_MARKER.len()].copy_from_slice(PLAINTEXT_MARKER);
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        payload,
    )
    .unwrap()
}

/// Every file under `root`, following no symlink.
fn walk(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                pending.push(entry.path());
            } else {
                found.push(entry.path());
            }
        }
    }
    found
}
