#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use conversation_memory::{MemoryStore, NeverCancelled, SqliteMemoryStore};
use conversation_model_adapters::{GenerationLanguageModel, GenerationLanguageRequest};
use conversation_protocol::{
    encode_gateway_message_for_version, GatewayMessage, GenerationId, MemoryConfidence,
    MemoryDraft, MemoryKind, MemoryProvenance, MemoryProvenanceKind, MemoryRetention,
    MemoryRetrievalRequest, SessionId, TurnId, UnixTimestampMillis,
    DEFAULT_CLIENT_PROTOCOL_VERSION as CLIENT_PROTOCOL_VERSION,
};
use conversation_runtime_gateway::{
    GatewayAdapters, GatewayConfig, GatewayDeploymentConfig, LanguageDeployment,
    MemoryExtractionSettings, ProviderEnvironmentPolicy, ProviderHost, ProviderHostOwnership,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const VALID_CONFIG: &str = r#"schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
execution = "local"
provider = "local-language"
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
"#;

const EXTERNAL_HOST: &str = r#"[[provider_hosts]]
id = "language-host"
ownership = "external"
readiness_url = "http://127.0.0.1:11434/api/tags"
startup_timeout_ms = 5000
environment = "inherit"
"#;

const GATEWAY_OWNED_HOST: &str = r#"[[provider_hosts]]
id = "language-host"
ownership = "gateway-owned"
readiness_url = "http://127.0.0.1:11434/api/tags"
startup_timeout_ms = 5000
environment = "clear"
executable = "/usr/bin/provider-host"
argv = ["serve", "--host", "127.0.0.1"]
"#;

const PROVIDER_READINESS_LIMIT_BYTES: usize = 2_048;

#[test]
fn schema_v2_external_host_is_validated_and_carried_without_changing_display_labels() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &schema_v2_config(EXTERNAL_HOST, "language-host"),
    );

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(
        adapters.status.components[0].provider_label,
        "local-language"
    );
    assert_eq!(adapters.provider_hosts().len(), 1);
    let host = &adapters.provider_hosts()[0];
    assert_eq!(host.id(), "language-host");
    assert_eq!(host.ownership(), ProviderHostOwnership::External);
    assert_eq!(host.readiness_url(), "http://127.0.0.1:11434/api/tags");
    assert_eq!(host.startup_timeout_ms(), 5000);
    assert_eq!(host.environment(), ProviderEnvironmentPolicy::Inherit);
    assert_eq!(host.executable(), None);
    assert_eq!(host.argv(), None);
}

#[test]
fn schema_v2_gateway_owned_host_carries_literal_launch_plan_without_spawning() {
    let fixture = tempfile::tempdir().unwrap();
    let spawn_marker = fixture.path().join("provider-spawned");
    let host = GATEWAY_OWNED_HOST.replace("/usr/bin/provider-host", &toml_path(&spawn_marker));
    let path = write_config(fixture.path(), &schema_v2_config(&host, "language-host"));

    let adapters = GatewayConfig::load(&path).unwrap();

    let host = &adapters.provider_hosts()[0];
    assert_eq!(host.ownership(), ProviderHostOwnership::GatewayOwned);
    assert_eq!(host.environment(), ProviderEnvironmentPolicy::Clear);
    assert_eq!(host.executable(), Some(spawn_marker.as_path()));
    assert_eq!(
        host.argv()
            .unwrap()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["serve", "--host", "127.0.0.1"]
    );
    assert!(!spawn_marker.exists());
}

#[test]
fn schema_v2_rejects_duplicate_or_invalid_provider_host_ids() {
    for hosts in [
        format!("{EXTERNAL_HOST}\n{EXTERNAL_HOST}"),
        EXTERNAL_HOST.replace("language-host", ""),
        EXTERNAL_HOST.replace("language-host", " language-host"),
        EXTERNAL_HOST.replace("language-host", &"x".repeat(129)),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(fixture.path(), &schema_v2_config(&hosts, "language-host"));

        assert!(
            GatewayConfig::load(&path).is_err(),
            "accepted hosts:\n{hosts}"
        );
    }
}

#[test]
fn schema_v2_rejects_invalid_readiness_urls_and_startup_timeouts() {
    for host in [
        EXTERNAL_HOST.replace("http://127.0.0.1:11434", "https://127.0.0.1:11434"),
        EXTERNAL_HOST.replace("127.0.0.1", "localhost"),
        EXTERNAL_HOST.replace("127.0.0.1", "192.0.2.1"),
        EXTERNAL_HOST.replace("startup_timeout_ms = 5000", "startup_timeout_ms = 99"),
        EXTERNAL_HOST.replace("startup_timeout_ms = 5000", "startup_timeout_ms = 120001"),
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(fixture.path(), &schema_v2_config(&host, "language-host"));

        assert!(
            GatewayConfig::load(&path).is_err(),
            "accepted host:\n{host}"
        );
    }
}

#[test]
fn provider_readiness_url_enforces_the_documented_utf8_byte_bound() {
    let boundary = readiness_url_with_bytes(PROVIDER_READINESS_LIMIT_BYTES);
    assert_eq!(boundary.len(), PROVIDER_READINESS_LIMIT_BYTES);
    assert!(ProviderHost::external(
        "language-host",
        boundary,
        5000,
        ProviderEnvironmentPolicy::Inherit,
    )
    .is_ok());

    let oversized = readiness_url_with_bytes(PROVIDER_READINESS_LIMIT_BYTES + 1);
    assert!(ProviderHost::external(
        "language-host",
        oversized,
        5000,
        ProviderEnvironmentPolicy::Inherit,
    )
    .is_err());
}

#[test]
fn schema_v2_enforces_external_and_gateway_owned_launch_fields() {
    let external_with_executable =
        format!("{EXTERNAL_HOST}executable = \"/usr/bin/provider-host\"\n");
    let external_with_argv = format!("{EXTERNAL_HOST}argv = []\n");
    let owned_without_executable = GATEWAY_OWNED_HOST
        .lines()
        .filter(|line| !line.starts_with("executable ="))
        .collect::<Vec<_>>()
        .join("\n");
    let owned_without_argv = GATEWAY_OWNED_HOST
        .lines()
        .filter(|line| !line.starts_with("argv ="))
        .collect::<Vec<_>>()
        .join("\n");
    let owned_with_relative_executable =
        GATEWAY_OWNED_HOST.replace("/usr/bin/provider-host", "relative/provider-host");

    for host in [
        external_with_executable,
        external_with_argv,
        owned_without_executable,
        owned_without_argv,
        owned_with_relative_executable,
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(fixture.path(), &schema_v2_config(&host, "language-host"));

        assert!(
            GatewayConfig::load(&path).is_err(),
            "accepted host:\n{host}"
        );
    }
}

#[test]
fn schema_v2_enforces_literal_argv_bounds() {
    let too_many = (0..33)
        .map(|index| format!("\"arg-{index}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let oversized = "x".repeat(4097);
    let aggregate = [
        "a".repeat(4096),
        "b".repeat(4096),
        "c".repeat(4096),
        "d".repeat(4096),
        "e".to_owned(),
    ];
    let aggregate = aggregate
        .iter()
        .map(|argument| format!("\"{argument}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let nul = "\\u0000";

    for argv in [
        format!("[{too_many}]"),
        format!("[\"{oversized}\"]"),
        format!("[{aggregate}]"),
        format!("[\"{nul}\"]"),
    ] {
        let host = GATEWAY_OWNED_HOST.replace("[\"serve\", \"--host\", \"127.0.0.1\"]", &argv);
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(fixture.path(), &schema_v2_config(&host, "language-host"));

        assert!(GatewayConfig::load(&path).is_err(), "accepted argv {argv}");
    }
}

#[test]
fn schema_v2_requires_a_declared_language_host_reference() {
    let fixture = tempfile::tempdir().unwrap();
    let missing_language_reference = write_config(
        fixture.path(),
        &schema_v2_config(EXTERNAL_HOST, "missing-host"),
    );
    assert!(GatewayConfig::load(&missing_language_reference).is_err());
}

#[cfg(unix)]
#[test]
fn schema_v2_voice_requires_a_speech_host_reference() {
    let fixture = GatewayFixture::voice(false);
    let voice_v2 = schema_v2_voice_config(&fixture, None, EXTERNAL_HOST);
    let path = write_config(fixture.directory(), &voice_v2);

    assert!(GatewayConfig::load(&path).is_err());
    assert!(!fixture.sidecar_spawned());
}

#[cfg(unix)]
#[test]
fn schema_v2_voice_speech_host_must_reference_a_declared_host() {
    let fixture = GatewayFixture::voice(false);
    let voice_v2 = schema_v2_voice_config(&fixture, Some("missing-speech-host"), EXTERNAL_HOST);
    let path = write_config(fixture.directory(), &voice_v2);

    assert!(GatewayConfig::load(&path).is_err());
    assert!(!fixture.sidecar_spawned());
}

#[cfg(unix)]
#[test]
fn schema_v2_voice_accepts_a_declared_speech_host_reference() {
    let fixture = GatewayFixture::voice(false);
    let speech_host = EXTERNAL_HOST
        .replace("language-host", "speech-host")
        .replace("11434/api/tags", "8000/health");
    let hosts = format!("{EXTERNAL_HOST}\n{speech_host}");
    let voice_v2 = schema_v2_voice_config(&fixture, Some("speech-host"), &hosts);
    let path = write_config(fixture.directory(), &voice_v2);

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(adapters.provider_hosts().len(), 2);
    assert!(!fixture.sidecar_spawned());
}

#[test]
fn typed_deployment_builder_serializes_deterministically_to_strict_toml() {
    let language = LanguageDeployment::ollama_compatible(
        "local-language",
        "http://127.0.0.1:11434",
        "private-model-id",
        "language-host",
    );
    let first = GatewayDeploymentConfig::builder(language.clone())
        .provider_host(
            ProviderHost::external(
                "z-host",
                "http://127.0.0.1:9000/ready",
                120_000,
                ProviderEnvironmentPolicy::Clear,
            )
            .unwrap(),
        )
        .provider_host(
            ProviderHost::external(
                "language-host",
                "http://127.0.0.1:11434/api/tags",
                100,
                ProviderEnvironmentPolicy::Inherit,
            )
            .unwrap(),
        )
        .to_toml()
        .unwrap();
    let second = GatewayDeploymentConfig::builder(language)
        .provider_host(
            ProviderHost::external(
                "language-host",
                "http://127.0.0.1:11434/api/tags",
                100,
                ProviderEnvironmentPolicy::Inherit,
            )
            .unwrap(),
        )
        .provider_host(
            ProviderHost::external(
                "z-host",
                "http://127.0.0.1:9000/ready",
                120_000,
                ProviderEnvironmentPolicy::Clear,
            )
            .unwrap(),
        )
        .to_toml()
        .unwrap();

    assert_eq!(first, second);
    assert!(first.starts_with("schema_version = 2\nprivacy_mode = \"local-only\"\n"));
    assert!(first.find("id = \"language-host\"").unwrap() < first.find("id = \"z-host\"").unwrap());
    assert!(first.contains("provider_host = \"language-host\""));

    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), &first);
    assert!(GatewayConfig::load(&path).is_ok());

    let unknown = first.replacen(
        "startup_timeout_ms = 100",
        "startup_timeout_ms = 100\nunknown = true",
        1,
    );
    let path = write_config(fixture.path(), &unknown);
    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn every_typed_serializer_success_is_accepted_by_the_exact_loader_limit() {
    const LOADER_LIMIT_BYTES: usize = 64 * 1024;

    let fixture = tempfile::tempdir().unwrap();
    let readiness_url = readiness_url_with_bytes(PROVIDER_READINESS_LIMIT_BYTES);
    let mut largest_accepted = 0;
    let mut saw_size_rejection = false;

    for host_count in 1..=40 {
        let mut deployment =
            GatewayDeploymentConfig::builder(LanguageDeployment::ollama_compatible(
                "local-language",
                "http://127.0.0.1:11434",
                "private-model-id",
                "language-host",
            ))
            .provider_host(
                ProviderHost::external(
                    "language-host",
                    &readiness_url,
                    5000,
                    ProviderEnvironmentPolicy::Inherit,
                )
                .unwrap(),
            );
        for index in 1..host_count {
            deployment = deployment.provider_host(
                ProviderHost::external(
                    format!("provider-host-{index:02}"),
                    &readiness_url,
                    5000,
                    ProviderEnvironmentPolicy::Clear,
                )
                .unwrap(),
            );
        }

        match deployment.to_toml() {
            Ok(contents) => {
                assert!(contents.len() <= LOADER_LIMIT_BYTES);
                largest_accepted = largest_accepted.max(contents.len());
                let path = write_config(fixture.path(), &contents);
                assert!(GatewayConfig::load(&path).is_ok());
            }
            Err(error) => {
                assert!(error.to_string().contains("64 KiB"));
                saw_size_rejection = true;
            }
        }
    }

    assert!(largest_accepted > 60 * 1024);
    assert!(saw_size_rejection);
}

#[test]
fn deployment_errors_do_not_echo_model_ids_or_private_paths() {
    let private_model = format!("private-model-{}", "x".repeat(300));
    let model_error = GatewayDeploymentConfig::builder(LanguageDeployment::ollama_compatible(
        "local-language",
        "http://127.0.0.1:11434",
        &private_model,
        "language-host",
    ))
    .provider_host(
        ProviderHost::external(
            "language-host",
            "http://127.0.0.1:11434/api/tags",
            5000,
            ProviderEnvironmentPolicy::Inherit,
        )
        .unwrap(),
    )
    .to_toml()
    .unwrap_err();
    assert!(!model_error.to_string().contains(&private_model));

    let private_path = "private/provider/path";
    let path_error = ProviderHost::gateway_owned(
        "language-host",
        "http://127.0.0.1:11434/api/tags",
        5000,
        ProviderEnvironmentPolicy::Clear,
        private_path,
        vec!["serve".to_owned()],
    )
    .unwrap_err();
    assert!(!path_error.to_string().contains(private_path));
}

#[test]
fn accepts_an_optional_deployment_system_prompt() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &VALID_CONFIG.replacen(
            "thinking = false",
            "thinking = false\nsystem_prompt = \"Describe only deployment capabilities explicitly supplied here.\"",
            1,
        ),
    );

    let _: GatewayAdapters = GatewayConfig::load(&path).unwrap();
}

#[test]
fn rejects_an_oversized_deployment_system_prompt() {
    let fixture = tempfile::tempdir().unwrap();
    let prompt = "x".repeat(4 * 1024 + 1);
    let path = write_config(
        fixture.path(),
        &VALID_CONFIG.replacen(
            "thinking = false",
            &format!("thinking = false\nsystem_prompt = \"{prompt}\""),
            1,
        ),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn loads_an_explicit_valid_local_only_configuration() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), VALID_CONFIG);

    let adapters: GatewayAdapters = GatewayConfig::load(&path).unwrap();
    assert_eq!(adapters.status.model_id, "local-model-id");
    assert_eq!(
        adapters.status.capabilities,
        ["text", "conversation_context_seed", "persona_control"]
    );
    assert_eq!(adapters.status.components.len(), 1);
    assert_eq!(adapters.status.components[0].kind, "language_model");
    assert_eq!(
        adapters.status.components[0].provider_label,
        "local-language"
    );
    assert!(adapters.voice.is_none());
    assert!(adapters.provider_hosts().is_empty());
}

#[test]
fn rejects_a_relative_configuration_path() {
    let error = GatewayConfig::load(Path::new("gateway.toml")).unwrap_err();

    assert!(error.to_string().contains("absolute"));
}

#[test]
fn rejects_a_configuration_larger_than_64_kib() {
    let fixture = tempfile::tempdir().unwrap();
    let path = fixture.path().join("gateway.toml");
    std::fs::write(&path, vec![b'x'; 64 * 1024 + 1]).unwrap();

    let error = GatewayConfig::load(&path).unwrap_err();

    assert!(error.to_string().contains("64 KiB"));
}

#[test]
fn rejects_unknown_configuration_fields() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &format!("{VALID_CONFIG}\nunexpected = true\n"),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn rejects_an_unsupported_schema_version() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &VALID_CONFIG.replacen("schema_version = 1", "schema_version = 2", 1),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn rejects_non_loopback_or_hostname_language_endpoints() {
    for endpoint in ["http://192.0.2.1:11434", "http://localhost:11434"] {
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(
            fixture.path(),
            &VALID_CONFIG.replacen("http://127.0.0.1:11434", endpoint, 1),
        );

        assert!(GatewayConfig::load(&path).is_err(), "accepted {endpoint}");
    }
}

#[test]
fn rejects_https_language_endpoints() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &VALID_CONFIG.replacen("http://127.0.0.1:11434", "https://127.0.0.1:11434", 1),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn rejects_language_endpoint_credentials_queries_and_fragments() {
    for endpoint in [
        "http://user:password@127.0.0.1:11434",
        "http://127.0.0.1:11434?token=value",
        "http://127.0.0.1:11434#fragment",
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(
            fixture.path(),
            &VALID_CONFIG.replacen("http://127.0.0.1:11434", endpoint, 1),
        );

        assert!(GatewayConfig::load(&path).is_err(), "accepted {endpoint}");
    }
}

#[test]
fn rejects_an_empty_model_identifier() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &VALID_CONFIG.replacen("model = \"local-model-id\"", "model = \"\"", 1),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn rejects_a_model_identifier_larger_than_256_bytes() {
    let fixture = tempfile::tempdir().unwrap();
    let oversized_model = "x".repeat(257);
    let path = write_config(
        fixture.path(),
        &VALID_CONFIG.replacen(
            "model = \"local-model-id\"",
            &format!("model = \"{oversized_model}\""),
            1,
        ),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[tokio::test]
async fn memory_configuration_returns_shared_retrieval_and_inspection_handles() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    let path = write_config(
        fixture.path(),
        &format!(
            "{VALID_CONFIG}\n[memory]\ndatabase = \"{}\"\nmaximum_items = 4\nmaximum_bytes = 4096\n",
            database.display()
        ),
    );

    let adapters: GatewayAdapters = GatewayConfig::load(&path).unwrap();
    let store = adapters.memory_store.as_ref().unwrap();
    let record = store
        .create(
            MemoryDraft::new(
                MemoryKind::Semantic,
                "shared gateway memory fixture",
                MemoryProvenance::new(
                    MemoryProvenanceKind::UserProvided,
                    "gateway-config-test",
                    UnixTimestampMillis::new(1_000).unwrap(),
                    "local-user",
                    None,
                )
                .unwrap(),
                MemoryConfidence::new(900).unwrap(),
                UnixTimestampMillis::new(1_000).unwrap(),
                MemoryRetention::UntilDeleted,
            )
            .unwrap(),
        )
        .unwrap();
    let retrieval = SqliteMemoryStore::open(&database)
        .unwrap()
        .retrieve(
            MemoryRetrievalRequest::new(
                TurnId::new(1),
                "shared gateway memory",
                UnixTimestampMillis::new(2_000).unwrap(),
                4,
                4_096,
            )
            .unwrap(),
            &NeverCancelled,
        )
        .unwrap();

    assert_eq!(retrieval.items().len(), 1);
    assert_eq!(retrieval.items()[0].memory_id(), record.id());
    assert_eq!(
        adapters.status.capabilities,
        [
            "text",
            "conversation_context_seed",
            "persona_control",
            "memory_inspection",
            "memory_mutation"
        ]
    );
    assert_eq!(adapters.status.components[1].kind, "memory");
}

#[test]
fn configuration_without_memory_returns_no_memory_handles() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), VALID_CONFIG);

    let adapters = GatewayConfig::load(&path).unwrap();

    assert!(adapters.memory_store.is_none());
    assert!(adapters.memory_extraction.is_none());
}

#[test]
fn memory_without_extraction_leaves_extraction_disabled() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    let path = write_config(fixture.path(), &memory_config(&database));

    let adapters = GatewayConfig::load(&path).unwrap();

    assert!(adapters.memory_store.is_some());
    assert!(adapters.memory_extraction.is_none());
}

#[test]
fn an_empty_extraction_table_applies_the_documented_defaults() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    let path = write_config(
        fixture.path(),
        &format!("{}\n[memory.extraction]\n", memory_config(&database)),
    );

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(
        adapters
            .memory_extraction
            .map(|extraction| extraction.settings),
        Some(MemoryExtractionSettings::new(3, 90))
    );
}

#[test]
fn extraction_settings_are_read_from_the_memory_extraction_table() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    let path = write_config(
        fixture.path(),
        &format!(
            "{}\n[memory.extraction]\nmax_memories_per_turn = 5\nepisodic_retention_days = 1\n",
            memory_config(&database)
        ),
    );

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(
        adapters
            .memory_extraction
            .map(|extraction| extraction.settings),
        Some(MemoryExtractionSettings::new(5, 1))
    );
}

/// The adapter prepends the deployment's system prompt to every request it is given,
/// which would seat the persona ahead of the extraction instruction and coax the model
/// into persona prose instead of the JSON array the parser needs. Asserted through the
/// wire, because the built model handle is otherwise opaque.
#[tokio::test]
async fn the_extraction_model_asks_without_the_persona_system_prompt() {
    const SYSTEM_PROMPT: &str = "You are a warm companion who always answers in prose.";
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("runtime.sqlite3");
    SqliteMemoryStore::initialize(&database).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let contents = memory_config(&database)
        .replacen(
            "thinking = false",
            &format!("thinking = false\nsystem_prompt = \"{SYSTEM_PROMPT}\""),
            1,
        )
        .replacen("http://127.0.0.1:11434", &endpoint, 1)
        + "\n[memory.extraction]\n";
    let path = write_config(fixture.path(), &contents);

    let adapters = GatewayConfig::load(&path).unwrap();
    let extraction = adapters
        .memory_extraction
        .expect("extraction is configured");

    let turn_request = ask(&listener, adapters.language.as_ref(), "the turn transcript").await;
    let extraction_request = ask(
        &listener,
        extraction.language.as_ref(),
        "the extraction prompt",
    )
    .await;

    assert!(
        turn_request.contains(SYSTEM_PROMPT),
        "the conversation model lost its system prompt: {turn_request}"
    );
    assert!(
        !extraction_request.contains(SYSTEM_PROMPT),
        "the extraction model carried the persona system prompt: {extraction_request}"
    );
    assert!(
        !extraction_request.contains(r#""role":"system""#),
        "the extraction model carried a system message: {extraction_request}"
    );
    assert!(
        extraction_request.contains("the extraction prompt"),
        "the extraction model dropped its instruction: {extraction_request}"
    );
    assert!(
        extraction_request.contains(r#""temperature":0.0"#),
        "the extraction model did not pin temperature: {extraction_request}"
    );
}

/// Drives one generation against `listener`, answering with an immediately-done Ollama
/// chat response, and returns the request body the model sent.
async fn ask(
    listener: &TcpListener,
    model: &dyn GenerationLanguageModel,
    transcript: &str,
) -> String {
    let mut deltas = model.stream(
        GenerationLanguageRequest::new(TurnId::new(1), GenerationId::new(1), transcript),
        CancellationToken::new(),
    );
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut request = Vec::new();
    let header_end = loop {
        if let Some(index) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        {
            break index;
        }
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&chunk[..count]);
    };
    let content_length = String::from_utf8_lossy(&request[..header_end])
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        let mut chunk = [0_u8; 4096];
        let count = stream.read(&mut chunk).await.unwrap();
        assert!(count > 0);
        request.extend_from_slice(&chunk[..count]);
    }
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n\
{\"message\":{\"role\":\"assistant\",\"content\":\"[]\"},\"done\":true}\n",
        )
        .await
        .unwrap();
    stream.flush().await.unwrap();
    while deltas.recv().await.is_some() {}
    String::from_utf8_lossy(&request[header_end..]).into_owned()
}

#[test]
fn rejects_extraction_settings_outside_their_bounds() {
    for setting in [
        "max_memories_per_turn = 0",
        "max_memories_per_turn = 6",
        "episodic_retention_days = 0",
        "unknown_extraction_setting = 1",
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let database = fixture.path().join("runtime.sqlite3");
        SqliteMemoryStore::initialize(&database).unwrap();
        let path = write_config(
            fixture.path(),
            &format!(
                "{}\n[memory.extraction]\n{setting}\n",
                memory_config(&database)
            ),
        );

        assert!(GatewayConfig::load(&path).is_err(), "accepted {setting}");
    }
}

#[test]
fn rejects_extraction_without_a_memory_table() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &format!("{VALID_CONFIG}\n[memory.extraction]\nmax_memories_per_turn = 3\n"),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[cfg(unix)]
#[test]
fn valid_voice_reuses_root_configuration_without_spawning() {
    let fixture = GatewayFixture::voice(true);

    let adapters = GatewayConfig::load(fixture.config()).unwrap();

    assert!(adapters.voice.is_some());
    assert_eq!(
        adapters.status.capabilities,
        [
            "text",
            "conversation_context_seed",
            "persona_control",
            "memory_inspection",
            "memory_mutation",
            "voice_session"
        ]
    );
    assert_eq!(
        adapters
            .status
            .components
            .iter()
            .map(|component| component.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "speech_recognition",
            "language_model",
            "speech_synthesis",
            "audio_io",
            "memory",
        ]
    );
    assert!(adapters
        .status
        .components
        .iter()
        .all(|component| component.execution_location == "local"));
    assert!(adapters.provider_hosts().is_empty());
    let voice = adapters.voice.as_ref().unwrap();
    let policy = voice.policy.for_session(SessionId::new(7)).unwrap();
    assert_eq!(policy.session_id(), SessionId::new(7));
    assert_eq!(policy.components().len(), 5);
    encode_gateway_message_for_version(
        &GatewayMessage::Ready {
            status: adapters.status.clone(),
        },
        CLIENT_PROTOCOL_VERSION,
    )
    .unwrap();
    let running_status = adapters.text_only_status();
    assert_eq!(
        running_status.capabilities,
        [
            "text",
            "conversation_context_seed",
            "persona_control",
            "memory_inspection",
            "memory_mutation"
        ]
    );
    assert_eq!(
        running_status
            .components
            .iter()
            .map(|component| component.kind.as_str())
            .collect::<Vec<_>>(),
        ["language_model", "memory"]
    );
    encode_gateway_message_for_version(
        &GatewayMessage::Ready {
            status: running_status,
        },
        CLIENT_PROTOCOL_VERSION,
    )
    .unwrap();
    assert!(!fixture.sidecar_spawned());
}

#[cfg(unix)]
#[test]
fn accepts_sensevoice_asr_backend_without_spawning() {
    let fixture = GatewayFixture::voice(false);
    let config =
        fixture
            .contents()
            .replacen("backend = \"whisperkit\"", "backend = \"sensevoice\"", 1);
    let path = write_config(fixture.directory(), &config);

    let adapters = GatewayConfig::load(&path).unwrap();

    assert!(adapters.voice.is_some());
    assert!(!fixture.sidecar_spawned());
}

#[cfg(unix)]
#[test]
fn rejects_unknown_asr_backends() {
    let fixture = GatewayFixture::voice(false);
    let config =
        fixture
            .contents()
            .replacen("backend = \"whisperkit\"", "backend = \"another-asr\"", 1);
    let path = write_config(fixture.directory(), &config);

    assert!(GatewayConfig::load(&path).is_err());
    assert!(!fixture.sidecar_spawned());
}

#[cfg(unix)]
#[test]
fn rejects_non_voice_configuration_inside_voice() {
    for section in [
        "language",
        "persona",
        "memory",
        "privacy",
        "tools",
        "telemetry",
    ] {
        let fixture = GatewayFixture::voice(false);
        let config = format!(
            "{}\n[voice.{section}]\nenabled = true\n",
            fixture.contents()
        );
        let path = write_config(fixture.directory(), &config);

        assert!(GatewayConfig::load(&path).is_err(), "accepted {section}");
        assert!(!fixture.sidecar_spawned());
    }
}

#[cfg(unix)]
#[test]
fn rejects_remote_execution_for_every_configured_component() {
    for provider in [
        "local-language",
        "local-speech-recognition",
        "local-speech-synthesis",
        "local-audio",
    ] {
        let fixture = GatewayFixture::voice(false);
        let config = fixture.contents().replacen(
            &format!("execution = \"local\"\nprovider = \"{provider}\""),
            &format!("execution = \"remote\"\nprovider = \"{provider}\""),
            1,
        );
        let path = write_config(fixture.directory(), &config);

        assert!(GatewayConfig::load(&path).is_err(), "accepted {provider}");
        assert!(!fixture.sidecar_spawned());
    }
}

#[cfg(unix)]
#[test]
fn rejects_non_loopback_speech_endpoints() {
    for endpoint in ["http://192.0.2.1:8000/v1", "http://localhost:8000/v1"] {
        let fixture = GatewayFixture::voice(false);
        let config = fixture
            .contents()
            .replacen("http://127.0.0.1:8000/v1", endpoint, 1);
        let path = write_config(fixture.directory(), &config);

        assert!(GatewayConfig::load(&path).is_err(), "accepted {endpoint}");
        assert!(!fixture.sidecar_spawned());
    }
}

#[cfg(unix)]
#[test]
fn rejects_missing_or_non_absolute_asr_model_paths() {
    for model_path in ["relative/model", "/definitely/missing/local-asr-model"] {
        let fixture = GatewayFixture::voice(false);
        let config = fixture.contents().replacen(
            &format!("model_path = \"{}\"", toml_path(fixture.model_path())),
            &format!("model_path = \"{model_path}\""),
            1,
        );
        let path = write_config(fixture.directory(), &config);

        assert!(GatewayConfig::load(&path).is_err(), "accepted {model_path}");
        assert!(!fixture.sidecar_spawned());
    }
}

#[cfg(unix)]
#[test]
fn rejects_invalid_sidecar_executables_without_spawning() {
    let cases = [
        ("relative/sidecar", true),
        ("/definitely/missing/voice-sidecar", true),
    ];
    for (sidecar, expected) in cases {
        let fixture = GatewayFixture::voice(false);
        let config = fixture.contents().replacen(
            &format!(
                "sidecar_executable = \"{}\"",
                toml_path(fixture.sidecar_path())
            ),
            &format!("sidecar_executable = \"{sidecar}\""),
            1,
        );
        let path = write_config(fixture.directory(), &config);

        assert_eq!(GatewayConfig::load(&path).is_err(), expected);
        assert!(!fixture.sidecar_spawned());
    }

    let fixture = GatewayFixture::voice(false);
    let non_executable = fixture.directory().join("not-executable");
    std::fs::write(&non_executable, "not executable").unwrap();
    let config = fixture.contents().replacen(
        &format!(
            "sidecar_executable = \"{}\"",
            toml_path(fixture.sidecar_path())
        ),
        &format!("sidecar_executable = \"{}\"", toml_path(&non_executable)),
        1,
    );
    let path = write_config(fixture.directory(), &config);

    assert!(GatewayConfig::load(&path).is_err());
    assert!(!fixture.sidecar_spawned());
}

#[cfg(unix)]
#[test]
fn provider_labels_enforce_trimmed_utf8_byte_bounds() {
    let valid_boundary = "é".repeat(64);
    let fixture = GatewayFixture::voice(false);
    let valid = fixture.contents().replacen(
        "provider = \"local-language\"",
        &format!("provider = \"{valid_boundary}\""),
        1,
    );
    let path = write_config(fixture.directory(), &valid);
    assert!(GatewayConfig::load(&path).is_ok());

    for invalid in [
        " local-language".to_owned(),
        "local-language ".to_owned(),
        String::new(),
        "x".repeat(129),
    ] {
        let fixture = GatewayFixture::voice(false);
        let config = fixture.contents().replacen(
            "provider = \"local-language\"",
            &format!("provider = \"{invalid}\""),
            1,
        );
        let path = write_config(fixture.directory(), &config);

        assert!(GatewayConfig::load(&path).is_err(), "accepted {invalid:?}");
        assert!(!fixture.sidecar_spawned());
    }
}

#[cfg(unix)]
#[test]
fn rejects_zero_adapter_and_memory_limits() {
    for (configured, zero) in [
        ("num_predict = 1024", "num_predict = 0"),
        ("num_ctx = 8192", "num_ctx = 0"),
        (
            "max_assistant_content_bytes = 65536",
            "max_assistant_content_bytes = 0",
        ),
        ("maximum_items = 4", "maximum_items = 0"),
        ("maximum_bytes = 4096", "maximum_bytes = 0"),
        ("max_tokens = 128", "max_tokens = 0"),
        ("max_text_bytes = 4096", "max_text_bytes = 0"),
        ("max_audio_bytes = 8388608", "max_audio_bytes = 0"),
        ("max_error_bytes = 65536", "max_error_bytes = 0"),
    ] {
        let fixture = GatewayFixture::voice(true);
        let config = fixture.contents().replacen(configured, zero, 1);
        let path = write_config(fixture.directory(), &config);

        assert!(GatewayConfig::load(&path).is_err(), "accepted {zero}");
        assert!(!fixture.sidecar_spawned());
    }
}

#[test]
fn rejects_a_relative_memory_database_path() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &format!("{VALID_CONFIG}\n[memory]\ndatabase = \"runtime.sqlite3\"\nmaximum_items = 4\nmaximum_bytes = 4096\n"),
    );

    assert!(GatewayConfig::load(&path).is_err());
}

#[test]
fn rejects_a_missing_memory_database() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("missing.sqlite3");
    let path = write_config(fixture.path(), &memory_config(&database));

    assert!(GatewayConfig::load(&path).is_err());
    assert!(!database.exists());
}

#[test]
fn rejects_an_uninitialized_memory_database() {
    let fixture = tempfile::tempdir().unwrap();
    let database = fixture.path().join("empty.sqlite3");
    std::fs::File::create(&database).unwrap();
    let path = write_config(fixture.path(), &memory_config(&database));

    assert!(GatewayConfig::load(&path).is_err());
}

#[cfg(unix)]
#[test]
fn rejects_a_leaf_configuration_symlink_before_reading_its_target() {
    let fixture = tempfile::tempdir().unwrap();
    let target = fixture.path().join("target.toml");
    std::fs::write(&target, VALID_CONFIG).unwrap();
    let link = fixture.path().join("gateway.toml");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let error = GatewayConfig::load(&link).unwrap_err();

    assert_eq!(
        error.to_string(),
        "gateway configuration file could not be opened"
    );
}

#[cfg(unix)]
#[test]
fn rejects_a_fifo_configuration_before_opening_it() {
    use std::sync::mpsc;
    use std::time::Duration;

    let fixture = tempfile::tempdir().unwrap();
    let fifo = fixture.path().join("gateway.toml");
    assert!(std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .unwrap()
        .success());
    let (sender, receiver) = mpsc::sync_channel(1);
    let loader_path = fifo.clone();
    std::thread::spawn(move || {
        sender
            .send(GatewayConfig::load(&loader_path).is_err())
            .unwrap();
    });

    assert_eq!(receiver.recv_timeout(Duration::from_millis(100)), Ok(true));
    assert_eq!(
        GatewayConfig::load(&fifo).unwrap_err().to_string(),
        "gateway configuration file could not be opened"
    );
}

fn memory_config(database: &Path) -> String {
    format!(
        "{VALID_CONFIG}\n[memory]\ndatabase = \"{}\"\nmaximum_items = 4\nmaximum_bytes = 4096\n",
        database.display()
    )
}

fn schema_v2_config(hosts: &str, language_host: &str) -> String {
    format!(
        "{}\n{hosts}",
        VALID_CONFIG
            .replacen("schema_version = 1", "schema_version = 2", 1)
            .replacen(
                "provider = \"local-language\"",
                &format!("provider = \"local-language\"\nprovider_host = \"{language_host}\""),
                1,
            )
    )
}

fn readiness_url_with_bytes(total_bytes: usize) -> String {
    const PREFIX: &str = "http://127.0.0.1/";
    assert!(total_bytes >= PREFIX.len());
    format!("{PREFIX}{}", "r".repeat(total_bytes - PREFIX.len()))
}

#[cfg(unix)]
fn schema_v2_voice_config(
    fixture: &GatewayFixture,
    speech_host: Option<&str>,
    hosts: &str,
) -> String {
    let mut config = fixture
        .contents()
        .replacen("schema_version = 1", "schema_version = 2", 1)
        .replacen(
            "provider = \"local-language\"",
            "provider = \"local-language\"\nprovider_host = \"language-host\"",
            1,
        );
    if let Some(speech_host) = speech_host {
        config = config.replacen(
            "provider = \"local-speech-synthesis\"",
            &format!("provider = \"local-speech-synthesis\"\nprovider_host = \"{speech_host}\""),
            1,
        );
    }
    format!("{config}\n{hosts}")
}

#[cfg(unix)]
struct GatewayFixture {
    directory: tempfile::TempDir,
    config: PathBuf,
    model_path: PathBuf,
    sidecar_path: PathBuf,
    spawn_marker: PathBuf,
    contents: String,
}

#[cfg(unix)]
impl GatewayFixture {
    fn voice(with_memory: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let model_path = directory.path().join("asr-model");
        std::fs::create_dir(&model_path).unwrap();
        let spawn_marker = directory.path().join("sidecar-spawned");
        let sidecar_path = directory.path().join("voice-sidecar");
        std::fs::write(
            &sidecar_path,
            format!("#!/bin/sh\nprintf spawned > '{}'\n", spawn_marker.display()),
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&sidecar_path).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&sidecar_path, permissions).unwrap();

        let memory = if with_memory {
            let database = directory.path().join("runtime.sqlite3");
            SqliteMemoryStore::initialize(&database).unwrap();
            format!(
                "\n[memory]\ndatabase = \"{}\"\nmaximum_items = 4\nmaximum_bytes = 4096\n",
                toml_path(&database)
            )
        } else {
            String::new()
        };
        let contents = format!(
            r#"{VALID_CONFIG}{memory}
[voice.capture]
device = "system-default"

[voice.turn]
speech_start_ms = 200
final_silence_ms = 600

[voice.asr]
backend = "whisperkit"
execution = "local"
provider = "local-speech-recognition"
model_path = "{}"
download = false

[voice.speech]
backend = "openai-compatible"
execution = "local"
provider = "local-speech-synthesis"
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
sidecar_executable = "{}"
max_error_bytes = 65536
"#,
            toml_path(&model_path),
            toml_path(&sidecar_path),
        );
        let config = write_config(directory.path(), &contents);
        Self {
            directory,
            config,
            model_path,
            sidecar_path,
            spawn_marker,
            contents,
        }
    }

    fn config(&self) -> &Path {
        &self.config
    }

    fn directory(&self) -> &Path {
        self.directory.path()
    }

    fn model_path(&self) -> &Path {
        &self.model_path
    }

    fn sidecar_path(&self) -> &Path {
        &self.sidecar_path
    }

    fn contents(&self) -> &str {
        &self.contents
    }

    fn sidecar_spawned(&self) -> bool {
        self.spawn_marker.exists()
    }
}

fn toml_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\")
}

fn write_config(directory: &Path, contents: &str) -> PathBuf {
    let path = directory.join("gateway.toml");
    std::fs::write(&path, contents).unwrap();
    path
}

// ---------------------------------------------------------------------------
// Schema v3: explicit profile selection, remote providers by credential
// environment reference, and the encrypted recording section.
// ---------------------------------------------------------------------------

/// Deliberately fake. Nothing in the repository may contain a real credential,
/// and the redaction test asserts this exact string never reaches a diagnostic.
const ONLINE_CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const ONLINE_CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_CREDENTIAL";
const RECORDING_KEY_ENV: &str = "CONVERSATION_TEST_RECORDING_KEY";

const ONLINE_CONFIG: &str = r#"schema_version = 3
profile = "online"
privacy_mode = "cloud"

[language]
backend = "openai-compatible"
execution = "remote"
provider = "openai"
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-text-model"
credential_env = "CONVERSATION_TEST_ONLINE_CREDENTIAL"
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
provider = "openai-streaming"
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-transcription-model"
credential_env = "CONVERSATION_TEST_ONLINE_CREDENTIAL"

[voice.speech]
backend = "openai-compatible"
execution = "remote"
provider = "openai-compatible-streaming"
mode = "streaming"
streaming_interval = 0.2
endpoint = "https://api.example.invalid/v1"
model = "operator-supplied-tts-model"
voice = "operator-supplied-voice"
credential_env = "CONVERSATION_TEST_ONLINE_CREDENTIAL"
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
sidecar_executable = "/absolute/path/to/conversation-voice-sidecar"
max_error_bytes = 65536
"#;

fn set_online_credential() {
    std::env::set_var(ONLINE_CREDENTIAL_ENV, ONLINE_CREDENTIAL_VALUE);
}

fn schema_v3_offline_config() -> String {
    schema_v2_config(EXTERNAL_HOST, "language-host").replacen(
        "schema_version = 2",
        "schema_version = 3",
        1,
    )
}

fn component(status: &conversation_protocol::RuntimeStatus, kind: &str) -> (String, String) {
    let component = status
        .components
        .iter()
        .find(|component| component.kind == kind)
        .unwrap_or_else(|| panic!("status carries no {kind} component"));
    (
        component.execution_location.clone(),
        component.provider_label.clone(),
    )
}

#[test]
fn an_omitted_profile_selects_offline_on_every_schema_version() {
    for contents in [VALID_CONFIG.to_owned(), schema_v3_offline_config()] {
        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(fixture.path(), &contents);

        let status = GatewayConfig::validate(&path).unwrap();

        assert_eq!(status.profile, "offline");
        assert_eq!(status.privacy_mode, "local_only");
        assert_eq!(status.language_location, "local");
        assert_eq!(component(&status, "language_model").0, "local");
        assert_eq!(GatewayConfig::load(&path).unwrap().status, status);
    }
}

/// Catches the production break where merely adding `[media]` to the decoder
/// silently started a listener for the established offline configuration, or
/// let legacy schema versions opt into an unnegotiated transport.
#[test]
fn schema_v3_media_is_optional_and_legacy_schemas_reject_it() {
    let fixture = tempfile::tempdir().unwrap();
    let omitted = write_config(fixture.path(), &schema_v3_offline_config());
    assert!(GatewayConfig::load(&omitted).unwrap().media.is_none());

    let media = r#"
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
    let configured = write_config(
        fixture.path(),
        &format!("{}{}", schema_v3_offline_config(), media),
    );
    assert_eq!(
        GatewayConfig::load(&configured)
            .unwrap()
            .media
            .unwrap()
            .advertised_endpoint(),
        "tcp://127.0.0.1:0"
    );
    assert_eq!(
        GatewayConfig::load(&configured)
            .unwrap()
            .media
            .unwrap()
            .packet_read_timeout(),
        std::time::Duration::from_millis(5_000),
        "omitting the inbound packet bound selects its safe default"
    );

    let configured_timeout = media.replace(
        "max_outbound_queue_ms = 10000",
        "max_outbound_queue_ms = 10000\npacket_read_timeout_ms = 2500",
    );
    let configured_timeout = write_config(
        fixture.path(),
        &format!("{}{}", schema_v3_offline_config(), configured_timeout),
    );
    assert_eq!(
        GatewayConfig::load(&configured_timeout)
            .unwrap()
            .media
            .unwrap()
            .packet_read_timeout(),
        std::time::Duration::from_millis(2_500)
    );

    // This validates references only. It deliberately never binds a private
    // address or reads certificate material in an automated test.
    let tls_private = media
        .replace("127.0.0.1:0", "192.168.50.10:9443")
        .replace("tcp://", "tls://")
        + "tls_certificate_path = \"/var/lib/gateway/media-cert.pem\"\n"
        + "tls_private_key_path = \"/var/lib/gateway/media-key.pem\"\n";
    let tls_configured = write_config(
        fixture.path(),
        &format!("{}{}", schema_v3_offline_config(), tls_private),
    );
    assert!(GatewayConfig::validate(&tls_configured).is_ok());

    for legacy in [
        VALID_CONFIG.to_owned(),
        schema_v2_config(EXTERNAL_HOST, "language-host"),
    ] {
        let path = write_config(fixture.path(), &format!("{legacy}{media}"));
        let error = GatewayConfig::validate(&path).unwrap_err().to_string();
        assert!(error.contains("schema_version 3"), "{error}");
    }
}

/// Catches the production break where an Internet-reachable plaintext media
/// listener is accepted because only the TCP bind succeeds at runtime.
#[test]
fn media_configuration_rejects_unbounded_or_unprotected_endpoints() {
    let fixture = tempfile::tempdir().unwrap();
    let base = schema_v3_offline_config();
    for media in [
        r#"
[media]
bind = "0.0.0.0:9000"
advertised_endpoint = "tcp://0.0.0.0:9000"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
"#,
        r#"
[media]
bind = "127.0.0.1:9000"
advertised_endpoint = "tcp://127.0.0.1:9000"
credential_lifetime_ms = 0
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
"#,
        r#"
[media]
bind = "127.0.0.1:9000"
advertised_endpoint = "tls://127.0.0.1:9000"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
"#,
        r#"
[media]
bind = "127.0.0.1:9000"
advertised_endpoint = "tcp://127.0.0.1:9000"
credential_lifetime_ms = 10000
reconnect_window_ms = 20000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
"#,
        r#"
[media]
bind = "127.0.0.1:9000"
advertised_endpoint = "tcp://127.0.0.1:9000"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
packet_read_timeout_ms = 0
"#,
        r#"
[media]
bind = "127.0.0.1:9000"
advertised_endpoint = "tcp://127.0.0.1:9000"
credential_lifetime_ms = 60000
reconnect_window_ms = 10000
max_frames_per_second = 100
max_payload_bytes = 16384
max_outbound_queue_bytes = 262144
max_outbound_queue_ms = 10000
packet_read_timeout_ms = 60001
"#,
    ] {
        let path = write_config(fixture.path(), &format!("{base}{media}"));
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted:\n{media}"
        );
    }
}

#[test]
fn an_explicit_offline_profile_is_accepted_only_from_schema_version_three() {
    let online_key = "profile = \"online\"";
    let offline = schema_v3_offline_config().replacen(
        "schema_version = 3",
        "schema_version = 3\nprofile = \"offline\"",
        1,
    );
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), &offline);
    assert_eq!(GatewayConfig::validate(&path).unwrap().profile, "offline");

    let downgraded = offline.replacen("schema_version = 3", "schema_version = 2", 1);
    let path = write_config(fixture.path(), &downgraded);
    assert!(GatewayConfig::validate(&path).is_err());

    // An online profile is never reachable from a legacy schema version.
    let legacy_online = offline
        .replacen("schema_version = 3", "schema_version = 2", 1)
        .replacen("profile = \"offline\"", online_key, 1);
    let path = write_config(fixture.path(), &legacy_online);
    assert!(GatewayConfig::validate(&path).is_err());
}

#[test]
fn a_valid_online_configuration_reports_cloud_privacy_and_remote_components() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), ONLINE_CONFIG);

    let status = GatewayConfig::validate(&path).unwrap();

    assert_eq!(status.profile, "online");
    assert_eq!(status.privacy_mode, "cloud");
    assert_eq!(status.language_location, "remote");
    assert_eq!(status.model_id, "operator-supplied-text-model");
    assert_eq!(
        component(&status, "language_model"),
        ("remote".to_owned(), "openai".to_owned())
    );
    assert_eq!(
        component(&status, "speech_recognition"),
        ("remote".to_owned(), "openai-streaming".to_owned())
    );
    assert_eq!(
        component(&status, "speech_synthesis"),
        (
            "remote".to_owned(),
            "openai-compatible-streaming".to_owned()
        )
    );
    assert_eq!(
        component(&status, "audio_io"),
        ("local".to_owned(), "local-audio".to_owned())
    );
    assert_eq!(
        status.capabilities,
        [
            "text",
            "conversation_context_seed",
            "persona_control",
            "voice_session"
        ]
    );

    // The protocol layer must accept the status this configuration describes.
    encode_gateway_message_for_version(
        &GatewayMessage::Ready {
            status: status.clone(),
        },
        CLIENT_PROTOCOL_VERSION,
    )
    .unwrap();
}

/// Catches a Task 7 regression where an online configuration advertised a voice session even
/// though the only available I/O factory was the intentionally unavailable placeholder.
#[test]
fn an_online_voice_configuration_requires_a_media_bridge() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();
    let media_start = ONLINE_CONFIG.find("[media]").unwrap();
    let capture_start = ONLINE_CONFIG.find("[voice.capture]").unwrap();
    let without_media = format!(
        "{}{}",
        &ONLINE_CONFIG[..media_start],
        &ONLINE_CONFIG[capture_start..]
    );
    let path = write_config(fixture.path(), &without_media);

    let error = GatewayConfig::validate(&path).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("online voice requires a media bridge configuration"),
        "unexpected configuration result: {error}"
    );
}

/// Catches the online-builder regression that used the adapter's default output limit instead
/// of validating the configured limit when constructing remote language adapters.
#[test]
fn an_online_language_configuration_rejects_a_zero_output_byte_limit() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen(
            "max_assistant_content_bytes = 65536",
            "max_assistant_content_bytes = 0",
            1,
        ),
    );

    assert!(
        GatewayConfig::load(&path).is_err(),
        "a zero configured output limit must not be replaced by the adapter default"
    );
}

#[test]
fn an_online_text_only_configuration_reports_a_remote_language_model() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();
    let voice = ONLINE_CONFIG.find("[voice]").unwrap();
    let path = write_config(fixture.path(), &ONLINE_CONFIG[..voice]);

    let status = GatewayConfig::validate(&path).unwrap();

    assert_eq!(status.profile, "online");
    assert_eq!(status.language_location, "remote");
    assert_eq!(status.components.len(), 1);
    assert_eq!(
        component(&status, "language_model"),
        ("remote".to_owned(), "openai".to_owned())
    );
    assert_eq!(
        status.capabilities,
        ["text", "conversation_context_seed", "persona_control"]
    );
    encode_gateway_message_for_version(&GatewayMessage::Ready { status }, CLIENT_PROTOCOL_VERSION)
        .unwrap();
}

#[test]
fn unknown_profile_and_backend_values_fail_closed() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    for profile in ["meeting", "Online", "online ", ""] {
        let path = write_config(
            fixture.path(),
            &ONLINE_CONFIG.replacen(
                "profile = \"online\"",
                &format!("profile = \"{profile}\""),
                1,
            ),
        );
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted {profile:?}"
        );
    }

    for backend in ["openai", "openai_compatible", "OpenaiCompatible"] {
        let path = write_config(
            fixture.path(),
            &ONLINE_CONFIG.replacen(
                "backend = \"openai-compatible\"",
                &format!("backend = \"{backend}\""),
                1,
            ),
        );
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted {backend}"
        );
    }
}

#[test]
fn an_online_configuration_requires_every_named_credential_variable() {
    set_online_credential();
    let missing = "CONVERSATION_TEST_ABSENT_CREDENTIAL";
    std::env::remove_var(missing);

    for section in [
        "provider = \"openai\"",
        "provider = \"openai-streaming\"",
        "provider = \"openai-compatible-streaming\"",
    ] {
        // Replace only the credential reference that follows this provider label.
        let position = ONLINE_CONFIG.find(section).unwrap();
        let (head, tail) = ONLINE_CONFIG.split_at(position);
        let contents = format!("{head}{}", tail.replacen(ONLINE_CREDENTIAL_ENV, missing, 1));

        let fixture = tempfile::tempdir().unwrap();
        let path = write_config(fixture.path(), &contents);

        let error = GatewayConfig::validate(&path).unwrap_err().to_string();

        assert!(error.contains(missing), "for {section}: {error}");
        assert!(!error.contains(ONLINE_CREDENTIAL_VALUE));
    }
}

#[test]
fn an_online_configuration_rejects_endpoints_that_are_not_remote_https() {
    set_online_credential();
    for endpoint in [
        "http://api.example.invalid/v1",
        "https://127.0.0.1:8443/v1",
        "https://user:password@api.example.invalid/v1",
        "https://api.example.invalid/v1?token=value",
        "https://api.example.invalid/v1#fragment",
        "ftp://api.example.invalid/v1",
        "not-a-url",
    ] {
        let fixture = tempfile::tempdir().unwrap();
        let contents = ONLINE_CONFIG.replacen("https://api.example.invalid/v1", endpoint, 1);
        let path = write_config(fixture.path(), &contents);

        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted language endpoint {endpoint}"
        );

        let contents = ONLINE_CONFIG.replacen(
            "endpoint = \"https://api.example.invalid/v1\"\nmodel = \"operator-supplied-tts-model\"",
            &format!("endpoint = \"{endpoint}\"\nmodel = \"operator-supplied-tts-model\""),
            1,
        );
        let path = write_config(fixture.path(), &contents);

        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted speech endpoint {endpoint}"
        );

        let contents = ONLINE_CONFIG.replacen(
            "endpoint = \"https://api.example.invalid/v1\"\nmodel = \"operator-supplied-transcription-model\"",
            &format!(
                "endpoint = \"{endpoint}\"\nmodel = \"operator-supplied-transcription-model\""
            ),
            1,
        );
        let path = write_config(fixture.path(), &contents);

        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted ASR endpoint {endpoint}"
        );
    }

    let fixture = tempfile::tempdir().unwrap();
    let contents = ONLINE_CONFIG.replacen(
        "endpoint = \"https://api.example.invalid/v1\"\nmodel = \"operator-supplied-transcription-model\"\n",
        "",
        1,
    );
    let path = write_config(fixture.path(), &contents);
    assert!(
        GatewayConfig::validate(&path).is_err(),
        "accepted missing ASR endpoint"
    );
}

#[test]
fn an_offline_configuration_still_rejects_remote_endpoints_and_executions() {
    let fixture = tempfile::tempdir().unwrap();
    let contents = VALID_CONFIG.replacen(
        "endpoint = \"http://127.0.0.1:11434\"",
        "endpoint = \"https://api.example.invalid/v1\"",
        1,
    );
    let path = write_config(fixture.path(), &contents);
    assert!(GatewayConfig::validate(&path).is_err());

    let contents = VALID_CONFIG.replacen("execution = \"local\"", "execution = \"remote\"", 1);
    let path = write_config(fixture.path(), &contents);
    assert!(GatewayConfig::validate(&path).is_err());
}

#[test]
fn online_and_offline_profiles_reject_incompatible_privacy_and_locations() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    // Online with local-only privacy.
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen(
            "privacy_mode = \"cloud\"",
            "privacy_mode = \"local-only\"",
            1,
        ),
    );
    assert!(GatewayConfig::validate(&path).is_err());

    // Online with a hybrid privacy mode the runtime cannot report per component.
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen("privacy_mode = \"cloud\"", "privacy_mode = \"hybrid\"", 1),
    );
    assert!(GatewayConfig::validate(&path).is_err());

    // Online with a locally executed language model.
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen("execution = \"remote\"", "execution = \"local\"", 1),
    );
    assert!(GatewayConfig::validate(&path).is_err());

    // Online with remote audio; the media path stays local until it is built.
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen(
            "execution = \"local\"\nprovider = \"local-audio\"",
            "execution = \"remote\"\nprovider = \"local-audio\"",
            1,
        ),
    );
    assert!(GatewayConfig::validate(&path).is_err());

    // Offline with a cloud privacy mode.
    let path = write_config(
        fixture.path(),
        &schema_v3_offline_config().replacen(
            "privacy_mode = \"local-only\"",
            "privacy_mode = \"cloud\"",
            1,
        ),
    );
    assert!(GatewayConfig::validate(&path).is_err());
}

#[test]
fn a_backend_must_agree_with_the_execution_it_declares() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    // A local backend cannot be declared remote inside an online deployment.
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen(
            "backend = \"openai-streaming\"",
            "backend = \"whisperkit\"",
            1,
        ),
    );
    assert!(GatewayConfig::validate(&path).is_err());

    // The remote language backend cannot appear in an offline deployment.
    let path = write_config(
        fixture.path(),
        &schema_v3_offline_config().replacen(
            "backend = \"ollama-compatible\"",
            "backend = \"openai-compatible\"",
            1,
        ),
    );
    assert!(GatewayConfig::validate(&path).is_err());
}

#[test]
fn the_streaming_online_recognizer_forbids_local_model_material() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    for extra in [
        "model_path = \"/absolute/path/to/local-asr-model\"",
        "download = true",
        "download = false",
    ] {
        let path = write_config(
            fixture.path(),
            &ONLINE_CONFIG.replacen(
                "backend = \"openai-streaming\"",
                &format!("backend = \"openai-streaming\"\n{extra}"),
                1,
            ),
        );
        assert!(GatewayConfig::validate(&path).is_err(), "accepted {extra}");
    }
}

#[cfg(unix)]
#[test]
fn local_recognizers_still_require_a_model_path_and_disabled_download() {
    for removed in ["model_path = \"", "download = false"] {
        let fixture = GatewayFixture::voice(false);
        let contents: String = fixture
            .contents()
            .lines()
            .filter(|line| !line.starts_with(removed))
            .collect::<Vec<_>>()
            .join("\n");
        let path = write_config(fixture.directory(), &contents);

        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted a voice ASR section without {removed}"
        );
    }
}

#[test]
fn credential_environment_names_are_bounded_and_never_carry_a_value() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    for name in [
        "",
        "lowercase_name",
        "0LEADING_DIGIT",
        "HAS-DASH",
        "HAS SPACE",
    ] {
        let path = write_config(
            fixture.path(),
            &ONLINE_CONFIG.replace(ONLINE_CREDENTIAL_ENV, name),
        );
        assert!(GatewayConfig::validate(&path).is_err(), "accepted {name:?}");
    }

    let oversized = "X".repeat(129);
    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replace(ONLINE_CREDENTIAL_ENV, &oversized),
    );
    assert!(GatewayConfig::validate(&path).is_err());
}

#[test]
fn credential_values_never_reach_diagnostics_while_the_name_may() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    // A successful load: the value is nowhere in the status the runtime reports.
    let path = write_config(fixture.path(), ONLINE_CONFIG);
    let status = GatewayConfig::validate(&path).unwrap();
    let rendered = format!("{status:?}");
    assert!(!rendered.contains(ONLINE_CREDENTIAL_VALUE));
    assert!(!rendered.contains(ONLINE_CREDENTIAL_ENV));

    // Every failing shape: the value is nowhere in the operator diagnostic.
    for contents in [
        ONLINE_CONFIG.replacen("https://api.example.invalid/v1", "http://127.0.0.1:1/v1", 1),
        ONLINE_CONFIG.replacen(
            "privacy_mode = \"cloud\"",
            "privacy_mode = \"local-only\"",
            1,
        ),
        ONLINE_CONFIG.replacen("schema_version = 3", "schema_version = 2", 1),
        ONLINE_CONFIG.replacen("execution = \"remote\"", "execution = \"local\"", 1),
    ] {
        let path = write_config(fixture.path(), &contents);
        let error = GatewayConfig::validate(&path).unwrap_err().to_string();
        assert!(
            !error.contains(ONLINE_CREDENTIAL_VALUE),
            "leaked in {error}"
        );
    }
}

#[test]
fn schema_three_keys_are_refused_by_older_schema_versions() {
    set_online_credential();
    std::env::set_var(RECORDING_KEY_ENV, ONLINE_CREDENTIAL_VALUE);
    let base = schema_v2_config(EXTERNAL_HOST, "language-host");
    let fixture = tempfile::tempdir().unwrap();

    for addition in [
        format!("credential_env = \"{ONLINE_CREDENTIAL_ENV}\""),
        "profile = \"online\"".to_owned(),
    ] {
        let contents = base.replacen(
            "provider = \"local-language\"",
            &format!("provider = \"local-language\"\n{addition}"),
            1,
        );
        let path = write_config(fixture.path(), &contents);
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted {addition}"
        );
    }

    let contents = format!(
        "{base}\n[recording]\ndirectory = \"/absolute/private/path/recordings\"\nkey_env = \"{RECORDING_KEY_ENV}\"\n"
    );
    let path = write_config(fixture.path(), &contents);
    assert!(GatewayConfig::validate(&path).is_err());
}

#[test]
fn the_recording_section_requires_an_absolute_directory_and_a_present_key() {
    std::env::set_var(RECORDING_KEY_ENV, ONLINE_CREDENTIAL_VALUE);
    let fixture = tempfile::tempdir().unwrap();
    let base = schema_v3_offline_config();

    let accepted = format!(
        "{base}\n[recording]\ndirectory = \"/absolute/private/path/recordings\"\nkey_env = \"{RECORDING_KEY_ENV}\"\n"
    );
    let path = write_config(fixture.path(), &accepted);
    assert!(GatewayConfig::validate(&path).is_ok());

    // The loader hands the sink the directory and the variable name, never a key.
    let recording_directory = tempfile::tempdir().unwrap();
    let loadable = format!(
        "{base}\n[recording]\ndirectory = \"{}\"\nkey_env = \"{RECORDING_KEY_ENV}\"\n",
        recording_directory.path().display()
    );
    let path = write_config(fixture.path(), &loadable);
    let adapters = GatewayConfig::load(&path).unwrap();
    let settings = adapters
        .recording
        .as_ref()
        .expect("recording is configured");
    assert_eq!(settings.directory(), recording_directory.path());
    assert_eq!(settings.key_env(), RECORDING_KEY_ENV);
    assert!(!format!("{settings:?}").contains(ONLINE_CREDENTIAL_VALUE));

    // The default configuration has no recording section and records nothing.
    let path = write_config(fixture.path(), &base);
    assert!(GatewayConfig::load(&path).unwrap().recording.is_none());

    for (directory, key_env) in [
        ("relative/recordings", RECORDING_KEY_ENV),
        ("/absolute/../escaping/recordings", RECORDING_KEY_ENV),
        ("", RECORDING_KEY_ENV),
        (
            "/absolute/private/path/recordings",
            "CONVERSATION_TEST_ABSENT_RECORDING_KEY",
        ),
        ("/absolute/private/path/recordings", "lowercase"),
    ] {
        let contents =
            format!("{base}\n[recording]\ndirectory = \"{directory}\"\nkey_env = \"{key_env}\"\n");
        let path = write_config(fixture.path(), &contents);
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted {directory} / {key_env}"
        );
    }
}

#[test]
fn model_and_voice_identifiers_are_bounded_in_both_profiles() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();
    let oversized = "x".repeat(257);
    let oversized_voice = "v".repeat(129);

    for (needle, replacement) in [
        ("operator-supplied-text-model", oversized.as_str()),
        ("operator-supplied-transcription-model", oversized.as_str()),
        ("operator-supplied-tts-model", oversized.as_str()),
        ("operator-supplied-voice", oversized_voice.as_str()),
        ("operator-supplied-voice", ""),
        ("operator-supplied-tts-model", ""),
    ] {
        let path = write_config(
            fixture.path(),
            &ONLINE_CONFIG.replacen(needle, replacement, 1),
        );
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted {needle} = {replacement:?}"
        );
    }
}

#[test]
fn the_voice_response_mode_defaults_to_automatic_and_accepts_the_external_gate() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();

    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen("response_mode = \"external_gate\"\n", "", 1),
    );
    assert!(GatewayConfig::validate(&path).is_ok());

    let path = write_config(
        fixture.path(),
        &ONLINE_CONFIG.replacen(
            "response_mode = \"external_gate\"",
            "response_mode = \"automatic\"",
            1,
        ),
    );
    assert!(GatewayConfig::validate(&path).is_ok());

    for invalid in ["external-gate", "gated", ""] {
        let path = write_config(
            fixture.path(),
            &ONLINE_CONFIG.replacen("\"external_gate\"", &format!("\"{invalid}\""), 1),
        );
        assert!(
            GatewayConfig::validate(&path).is_err(),
            "accepted {invalid}"
        );
    }
}

#[test]
fn the_published_meeting_online_example_configuration_stays_valid() {
    std::env::set_var("OPENAI_API_KEY", ONLINE_CREDENTIAL_VALUE);
    std::env::set_var("MEETING_RECORDING_KEY", ONLINE_CREDENTIAL_VALUE);
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../configs/gateway.meeting-online.example.toml");

    let status = GatewayConfig::validate(&path).unwrap();

    assert_eq!(status.profile, "online");
    assert_eq!(status.privacy_mode, "cloud");
    assert_eq!(status.language_location, "remote");
    assert!(status
        .components
        .iter()
        .any(|component| component.execution_location == "remote"));
    encode_gateway_message_for_version(&GatewayMessage::Ready { status }, CLIENT_PROTOCOL_VERSION)
        .unwrap();
}

#[test]
fn the_published_offline_example_configuration_stays_offline() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../configs/gateway.example.toml");

    let status = GatewayConfig::validate(&path).unwrap();

    assert_eq!(status.profile, "offline");
    assert_eq!(status.privacy_mode, "local_only");
    assert_eq!(status.language_location, "local");
}

#[test]
fn the_online_profile_constructs_remote_adapters_without_a_local_fallback() {
    set_online_credential();
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), ONLINE_CONFIG);

    let adapters = GatewayConfig::load(&path).unwrap();

    assert_eq!(adapters.status.profile, "online");
    assert!(adapters.voice.is_some());
    assert!(adapters
        .status
        .components
        .iter()
        .any(|component| component.kind == "language_model"
            && component.execution_location == "remote"));
}
