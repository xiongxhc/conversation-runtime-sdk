#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};
    use std::path::Path;

    use super::{read_profile_contents, SpeechProfile, MAX_PROFILE_BYTES};

    fn write_profile(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(contents.as_bytes()).unwrap();
        file
    }

    #[test]
    fn loads_default_and_selected_multilingual_profiles() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "british"

[profiles.british]
backend = "macos-system"
voice = "Daniel"
rate_wpm = 190

[profiles.mandarin]
backend = "macos-system"
voice = "Tingting"
rate_wpm = 180
"#,
        );

        assert_eq!(
            SpeechProfile::load(file.path(), None).unwrap(),
            SpeechProfile::MacOsSystem {
                voice: Some("Daniel".to_owned()),
                rate_wpm: Some(190),
            }
        );
        assert!(matches!(
            SpeechProfile::load(file.path(), Some("mandarin")).unwrap(),
            SpeechProfile::MacOsSystem { voice: Some(voice), .. } if voice == "Tingting"
        ));
    }

    #[test]
    fn loads_valid_local_http_profile() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-model"
voice = "local-voice"
speed = 1.0
language = "Chinese"
instructions = "Warm and calm."
max_tokens = 128
repetition_penalty = 1.05
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_ok());
    }

    #[test]
    fn rejects_local_http_profile_without_model() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:8000/v1"
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_local_http_profile_with_invalid_endpoint() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "not a URL"
model = "local-model"
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_local_http_profile_with_empty_text_fields() {
        for field in ["voice", "language", "instructions"] {
            let file = write_profile(&format!(
                r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-model"
{field} = ""
"#,
            ));

            assert!(SpeechProfile::load(file.path(), None).is_err(), "{field}");
        }
    }

    #[test]
    fn rejects_local_http_profile_with_non_positive_speed() {
        for speed in ["0.0", "-1.0"] {
            let file = write_profile(&format!(
                r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-model"
speed = {speed}
"#,
            ));

            assert!(SpeechProfile::load(file.path(), None).is_err(), "{speed}");
        }
    }

    #[test]
    fn rejects_local_http_profile_with_zero_generation_token_limit() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-model"
max_tokens = 0
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_local_http_profile_with_non_positive_repetition_penalty() {
        for repetition_penalty in ["0.0", "-1.0"] {
            let file = write_profile(&format!(
                r#"
schema_version = 1
default_profile = "local-neural"

[profiles.local-neural]
backend = "openai-compatible"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-model"
repetition_penalty = {repetition_penalty}
"#,
            ));

            assert!(
                SpeechProfile::load(file.path(), None).is_err(),
                "{repetition_penalty}"
            );
        }
    }

    #[test]
    fn rejects_backend_incompatible_profile_fields() {
        for (backend, field) in [
            ("macos-system", "speed = 1.0"),
            ("openai-compatible", "rate_wpm = 180"),
        ] {
            let file = write_profile(&format!(
                r#"
schema_version = 1
default_profile = "selected"

[profiles.selected]
backend = "{backend}"
endpoint = "http://127.0.0.1:8000/v1"
model = "local-model"
{field}
"#,
            ));

            assert!(SpeechProfile::load(file.path(), None).is_err(), "{backend}");
        }
    }

    #[test]
    fn rejects_relative_profile_path() {
        assert!(SpeechProfile::load(Path::new("speech.toml"), None).is_err());
    }

    #[test]
    fn rejects_profile_file_larger_than_64_kib() {
        let file = write_profile(&format!("{}\n", "#".repeat(64 * 1024)));

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_reader_larger_than_64_kib() {
        let contents = vec![b'#'; (MAX_PROFILE_BYTES + 1) as usize];

        assert_eq!(
            read_profile_contents(Cursor::new(contents)).unwrap_err(),
            "speech profile file exceeded 64 KiB"
        );
    }

    #[test]
    fn rejects_malformed_profile_toml() {
        let file = write_profile("schema_version = [");

        let error = SpeechProfile::load(file.path(), None).unwrap_err();

        assert!(error.starts_with("speech profile file was not valid TOML:"));
        assert!(error.len() <= 256);
        assert!(!error.chars().any(char::is_control));
    }

    #[test]
    fn rejects_unsupported_schema_version() {
        let file = write_profile(
            r#"
schema_version = 2
default_profile = "system"

[profiles.system]
backend = "macos-system"
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_unknown_profile_fields() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "system"
unexpected = "value"

[profiles.system]
backend = "macos-system"
"#,
        );

        let error = SpeechProfile::load(file.path(), None).unwrap_err();

        assert!(error.contains("unknown field `unexpected`"));
        assert!(!error.chars().any(char::is_control));
    }

    #[test]
    fn rejects_missing_default_profile() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "missing"

[profiles.system]
backend = "macos-system"
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_unknown_selected_profile() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "system"

[profiles.system]
backend = "macos-system"
"#,
        );

        assert!(SpeechProfile::load(file.path(), Some("missing")).is_err());
    }

    #[test]
    fn rejects_unsupported_profile_backend() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "system"

[profiles.system]
backend = "other"
"#,
        );

        let error = SpeechProfile::load(file.path(), None).unwrap_err();

        assert!(error.contains("unknown variant `other`"));
        assert!(!error.chars().any(char::is_control));
    }

    #[test]
    fn rejects_empty_profile_voice() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "system"

[profiles.system]
backend = "macos-system"
voice = ""
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }

    #[test]
    fn rejects_zero_profile_rate() {
        let file = write_profile(
            r#"
schema_version = 1
default_profile = "system"

[profiles.system]
backend = "macos-system"
rate_wpm = 0
"#,
        );

        assert!(SpeechProfile::load(file.path(), None).is_err());
    }
}

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use conversation_model_adapters::OpenAiCompatibleSpeechConfig;
use serde::Deserialize;

const MAX_PROFILE_BYTES: u64 = 64 * 1024;
const MAX_TOML_ERROR_DETAIL_CHARS: usize = 160;

fn read_profile_contents(reader: impl Read) -> Result<String, String> {
    let mut contents = String::new();
    reader
        .take(MAX_PROFILE_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(|_| "failed to read speech profile file".to_owned())?;
    if contents.len() as u64 > MAX_PROFILE_BYTES {
        return Err("speech profile file exceeded 64 KiB".to_owned());
    }
    Ok(contents)
}

fn sanitized_toml_error_detail(error: toml::de::Error) -> String {
    let detail = error
        .to_string()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let mut characters = detail.chars();
    let truncated = characters
        .by_ref()
        .take(MAX_TOML_ERROR_DETAIL_CHARS)
        .collect::<String>();

    if characters.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SpeechProfilesFile {
    schema_version: u32,
    default_profile: String,
    profiles: BTreeMap<String, RawSpeechProfile>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "backend", deny_unknown_fields)]
enum RawSpeechProfile {
    #[serde(rename = "macos-system")]
    MacOsSystem {
        voice: Option<String>,
        rate_wpm: Option<u32>,
    },
    #[serde(rename = "openai-compatible")]
    OpenAiCompatible {
        endpoint: String,
        model: String,
        voice: Option<String>,
        speed: Option<f32>,
        language: Option<String>,
        instructions: Option<String>,
        max_tokens: Option<usize>,
        repetition_penalty: Option<f32>,
    },
}

#[derive(Debug, PartialEq)]
pub(crate) enum SpeechProfile {
    MacOsSystem {
        voice: Option<String>,
        rate_wpm: Option<u32>,
    },
    OpenAiCompatible {
        endpoint: String,
        model: String,
        voice: Option<String>,
        speed: Option<f32>,
        language: Option<String>,
        instructions: Option<String>,
        max_tokens: Option<usize>,
        repetition_penalty: Option<f32>,
    },
}

impl SpeechProfile {
    pub(crate) fn load(path: &Path, selected_id: Option<&str>) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("speech profile path must be absolute".to_owned());
        }
        let metadata = std::fs::metadata(path)
            .map_err(|_| "failed to inspect speech profile file".to_owned())?;
        if metadata.len() > MAX_PROFILE_BYTES {
            return Err("speech profile file exceeded 64 KiB".to_owned());
        }
        let file = std::fs::File::open(path)
            .map_err(|_| "failed to read speech profile file".to_owned())?;
        let contents = read_profile_contents(file)?;
        let profiles: SpeechProfilesFile = toml::from_str(&contents).map_err(|error| {
            format!(
                "speech profile file was not valid TOML: {}",
                sanitized_toml_error_detail(error)
            )
        })?;
        if profiles.schema_version != 1 {
            return Err("speech profile schema version must be 1".to_owned());
        }

        let profile_id = selected_id.unwrap_or(&profiles.default_profile);
        let profile = profiles
            .profiles
            .get(profile_id)
            .ok_or_else(|| format!("speech profile was not found: {profile_id}"))?;

        match profile {
            RawSpeechProfile::MacOsSystem { voice, rate_wpm } => {
                validate_macos_system_profile(voice.as_deref(), *rate_wpm)?;
                Ok(Self::MacOsSystem {
                    voice: voice.clone(),
                    rate_wpm: *rate_wpm,
                })
            }
            RawSpeechProfile::OpenAiCompatible {
                endpoint,
                model,
                voice,
                speed,
                language,
                instructions,
                max_tokens,
                repetition_penalty,
            } => {
                validate_openai_compatible_profile(
                    endpoint,
                    model,
                    voice.as_deref(),
                    *speed,
                    language.as_deref(),
                    instructions.as_deref(),
                    *max_tokens,
                    *repetition_penalty,
                )?;
                Ok(Self::OpenAiCompatible {
                    endpoint: endpoint.clone(),
                    model: model.clone(),
                    voice: voice.clone(),
                    speed: *speed,
                    language: language.clone(),
                    instructions: instructions.clone(),
                    max_tokens: *max_tokens,
                    repetition_penalty: *repetition_penalty,
                })
            }
        }
    }
}

fn validate_macos_system_profile(voice: Option<&str>, rate_wpm: Option<u32>) -> Result<(), String> {
    if voice.is_some_and(|voice| voice.is_empty() || voice.chars().any(char::is_control)) {
        return Err("voice must be non-empty and contain no control characters".to_owned());
    }
    if rate_wpm.is_some_and(|rate| rate == 0) {
        return Err("rate must be non-zero".to_owned());
    }
    Ok(())
}

fn validate_openai_compatible_profile(
    endpoint: &str,
    model: &str,
    voice: Option<&str>,
    speed: Option<f32>,
    language: Option<&str>,
    instructions: Option<&str>,
    max_tokens: Option<usize>,
    repetition_penalty: Option<f32>,
) -> Result<(), String> {
    let mut config = OpenAiCompatibleSpeechConfig::new(model).map_err(adapter_message)?;
    config = config.with_endpoint(endpoint).map_err(adapter_message)?;
    if let Some(voice) = voice {
        config = config.with_voice(voice).map_err(adapter_message)?;
    }
    if let Some(speed) = speed {
        config = config.with_speed(speed).map_err(adapter_message)?;
    }
    if let Some(language) = language {
        config = config.with_language(language).map_err(adapter_message)?;
    }
    if let Some(instructions) = instructions {
        config = config
            .with_instructions(instructions)
            .map_err(adapter_message)?;
    }
    if let Some(max_tokens) = max_tokens {
        config = config
            .with_max_tokens(max_tokens)
            .map_err(adapter_message)?;
    }
    if let Some(repetition_penalty) = repetition_penalty {
        config
            .with_repetition_penalty(repetition_penalty)
            .map_err(adapter_message)?;
    }
    Ok(())
}

fn adapter_message(error: conversation_model_adapters::AdapterError) -> String {
    error.message().to_owned()
}
