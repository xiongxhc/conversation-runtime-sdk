use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use conversation_memory::{
    MemoryStoreErrorKind, PeopleStore, SignatureKey, SignatureModel, SqliteMemoryStore,
};
use conversation_protocol::RuntimeError;
use conversation_runtime::{IdentityPolicy, SpeakerFuture, VoiceEmbedder};
use tokio_util::sync::CancellationToken;

use super::{GatewayConfig, SpeakerIdentityConfig};

/// Deliberately fake signature keys: 64 hexadecimal characters, nothing more.
const SPEAKER_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const OTHER_SPEAKER_KEY: &str = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210";

/// Values from the runtime's own test policy.
fn section(key_env: &str) -> String {
    format!(
        "key_env = \"{key_env}\"\nwait_ms = 1000\nconfident_score = 0.80\npossible_score = 0.60\nmargin = 0.10\nanchor_score = 0.85\nmin_sample_ms = 1500\nmaturity_speech_ms = 5000\nrefinement_cap = 3\n"
    )
}

fn initialized_store(directory: &Path) -> (PathBuf, SqliteMemoryStore) {
    let database = directory.join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    let store = SqliteMemoryStore::open(&database).unwrap();
    (database, store)
}

/// Schema 3, offline, with `[memory]` on `database` and a complete `[speaker_identity]`.
fn config(database: &Path, key_env: &str) -> String {
    format!(
        r#"schema_version = 3
privacy_mode = "local-only"

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

[[provider_hosts]]
id = "language-host"
ownership = "external"
readiness_url = "http://127.0.0.1:11434/api/tags"
startup_timeout_ms = 5000
environment = "inherit"

[memory]
database = "{}"
maximum_items = 4
maximum_bytes = 4096

[speaker_identity]
{}"#,
        database.to_string_lossy().replace('\\', "\\\\"),
        section(key_env)
    )
}

/// Stands in for the voice embedding model this build does not include.
struct StandInEmbedder {
    model: SignatureModel,
}

impl VoiceEmbedder for StandInEmbedder {
    fn model(&self) -> &SignatureModel {
        &self.model
    }

    fn embed(
        &self,
        _samples: Vec<f32>,
        _cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Vec<f32>, RuntimeError>> {
        Box::pin(async { Ok(vec![1.0, 0.0, 0.0, 0.0]) })
    }
}

fn stand_in_embedder() -> Arc<dyn VoiceEmbedder> {
    Arc::new(StandInEmbedder {
        model: SignatureModel::new("ecapa", "1").unwrap(),
    })
}

#[test]
fn settings_carry_the_key_the_policy_and_the_wait() {
    let key_env = "CONVERSATION_TEST_SPEAKER_IDENTITY_KEY_SETTINGS";
    std::env::set_var(key_env, SPEAKER_KEY);
    let fixture = tempfile::tempdir().unwrap();
    let (_, store) = initialized_store(fixture.path());
    let section: SpeakerIdentityConfig = toml::from_str(&section(key_env)).unwrap();

    let settings = section.settings(&store).unwrap();

    assert_eq!(
        settings.policy,
        IdentityPolicy {
            confident_score: 0.80,
            possible_score: 0.60,
            margin: 0.10,
            anchor_score: 0.85,
            min_sample_ms: 1_500,
            maturity_speech_ms: 5_000,
            refinement_cap: 3,
        }
    );
    assert_eq!(settings.wait, Duration::from_millis(1_000));
    // The carried key is the configured one, and the store is now bound to it.
    store.verify_signature_key(&settings.key).unwrap();
    store
        .verify_signature_key(&SignatureKey::from_hex(SPEAKER_KEY).unwrap())
        .unwrap();
    assert_eq!(
        store
            .verify_signature_key(&SignatureKey::from_hex(OTHER_SPEAKER_KEY).unwrap())
            .unwrap_err()
            .kind(),
        MemoryStoreErrorKind::KeyMismatch
    );
}

#[test]
fn a_test_voice_embedder_lets_the_section_load() {
    let key_env = "CONVERSATION_TEST_SPEAKER_IDENTITY_KEY_EMBEDDER";
    std::env::set_var(key_env, SPEAKER_KEY);
    let fixture = tempfile::tempdir().unwrap();
    let (database, _) = initialized_store(fixture.path());
    let path = fixture.path().join("gateway.toml");
    std::fs::write(&path, config(&database, key_env)).unwrap();

    // Without an embedder the section stops at the missing voice model.
    let expected =
        "speaker identity needs a voice embedding model, which this build does not include";
    assert_eq!(
        GatewayConfig::load(&path).unwrap_err().to_string(),
        expected
    );

    let adapters =
        GatewayConfig::load_with_test_voice_embedder(&path, stand_in_embedder()).unwrap();
    assert!(adapters.memory_store.is_some());

    // The stand-in removes only the last check; every earlier one still applies.
    let other = "CONVERSATION_TEST_SPEAKER_IDENTITY_KEY_EMBEDDER_OTHER";
    std::env::set_var(other, OTHER_SPEAKER_KEY);
    std::fs::write(&path, config(&database, other)).unwrap();
    assert_eq!(
        GatewayConfig::load_with_test_voice_embedder(&path, stand_in_embedder())
            .unwrap_err()
            .to_string(),
        "speaker identity key does not match the key this memory store was first used with"
    );
    std::fs::write(
        &path,
        config(&database, key_env).replacen("wait_ms = 1000", "wait_ms = 5001", 1),
    )
    .unwrap();
    assert_eq!(
        GatewayConfig::load_with_test_voice_embedder(&path, stand_in_embedder())
            .unwrap_err()
            .to_string(),
        "speaker identity wait_ms must be within 100..=5000"
    );
}
