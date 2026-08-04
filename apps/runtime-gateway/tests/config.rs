use std::path::{Path, PathBuf};

use conversation_memory::SqliteMemoryStore;
use conversation_runtime_gateway::{GatewayAdapters, GatewayConfig};

const VALID_CONFIG: &str = r#"schema_version = 1
privacy_mode = "local-only"

[language]
backend = "ollama-compatible"
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

#[test]
fn loads_an_explicit_valid_local_only_configuration() {
    let fixture = tempfile::tempdir().unwrap();
    let path = write_config(fixture.path(), VALID_CONFIG);

    let config = GatewayConfig::load(&path).unwrap();
    let adapters: GatewayAdapters = config.into_adapters().unwrap();
    assert_eq!(adapters.model_id(), "local-model-id");
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

#[test]
fn accepts_an_existing_initialized_absolute_memory_database() {
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

    let config = GatewayConfig::load(&path).unwrap();
    let _: GatewayAdapters = config.into_adapters().unwrap();
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

fn write_config(directory: &Path, contents: &str) -> PathBuf {
    let path = directory.join("gateway.toml");
    std::fs::write(&path, contents).unwrap();
    path
}
