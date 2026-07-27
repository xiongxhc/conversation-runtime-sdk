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
            SpeechProfile {
                voice: Some("Daniel".to_owned()),
                rate_wpm: Some(190),
            }
        );
        assert_eq!(
            SpeechProfile::load(file.path(), Some("mandarin"))
                .unwrap()
                .voice,
            Some("Tingting".to_owned())
        );
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
#[serde(deny_unknown_fields)]
struct RawSpeechProfile {
    backend: SpeechBackend,
    voice: Option<String>,
    rate_wpm: Option<u32>,
}

#[derive(Debug, Deserialize)]
enum SpeechBackend {
    #[serde(rename = "macos-system")]
    MacOsSystem,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct SpeechProfile {
    pub(crate) voice: Option<String>,
    pub(crate) rate_wpm: Option<u32>,
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
        let RawSpeechProfile {
            backend,
            voice,
            rate_wpm,
        } = profiles
            .profiles
            .get(profile_id)
            .ok_or_else(|| format!("speech profile was not found: {profile_id}"))?;
        match backend {
            SpeechBackend::MacOsSystem => {}
        }

        if voice
            .as_deref()
            .is_some_and(|voice| voice.is_empty() || voice.chars().any(char::is_control))
        {
            return Err("voice must be non-empty and contain no control characters".to_owned());
        }
        if rate_wpm.is_some_and(|rate| rate == 0) {
            return Err("rate must be non-zero".to_owned());
        }

        Ok(Self {
            voice: voice.clone(),
            rate_wpm: *rate_wpm,
        })
    }
}
