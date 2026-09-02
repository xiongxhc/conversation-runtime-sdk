//! The one place a provider credential is read, held, and redacted.
//!
//! Configuration names an environment variable; it never holds a value. The
//! value is read at the moment a request needs it, kept in a wrapper that
//! cannot be printed or serialized, and reaches nothing but an Authorization
//! header.

use std::fmt;

const MAX_CREDENTIAL_ENV_NAME_BYTES: usize = 128;

/// A provider credential.
///
/// Deliberately not `Debug`-derived, not `Clone`, not `Serialize`, and never
/// stored on a configuration: the value exists only for the lifetime of the
/// task that makes the request.
pub(crate) struct Credential(String);

impl Credential {
    /// Reads the named environment variable, returning `None` when it is unset
    /// or blank. The caller supplies its own diagnostic, so no message ever
    /// names the variable it read.
    pub(crate) fn read(name: &str) -> Option<Self> {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .map(Self)
    }

    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Credential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Credential(<redacted>)")
    }
}

/// Mirrors the gateway's rule for the *name* of a credential variable, so a
/// configuration the gateway accepts is a configuration every adapter accepts.
///
/// Returns the reason a name is unusable, so each adapter can phrase its own
/// configuration error without repeating the rule.
pub(crate) fn validate_credential_env_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > MAX_CREDENTIAL_ENV_NAME_BYTES {
        return Err("credential environment variable name must be within 1..=128 bytes");
    }
    if name.starts_with(|character: char| character.is_ascii_digit())
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(
            "credential environment variable name must be uppercase ASCII letters, digits, and underscores",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{validate_credential_env_name, Credential};

    #[test]
    fn credential_names_follow_the_gateway_rule() {
        assert!(validate_credential_env_name("ONLINE_TEXT_KEY_1").is_ok());
        assert!(validate_credential_env_name("").is_err());
        assert!(validate_credential_env_name("lowercase").is_err());
        assert!(validate_credential_env_name("1LEADING_DIGIT").is_err());
        assert!(validate_credential_env_name("HAS-DASH").is_err());
        assert!(validate_credential_env_name(&"A".repeat(129)).is_err());
    }

    #[test]
    fn a_blank_or_unset_variable_yields_no_credential() {
        std::env::set_var("CONVERSATION_TEST_CREDENTIAL_BLANK", "   ");
        std::env::remove_var("CONVERSATION_TEST_CREDENTIAL_UNSET");

        assert!(Credential::read("CONVERSATION_TEST_CREDENTIAL_BLANK").is_none());
        assert!(Credential::read("CONVERSATION_TEST_CREDENTIAL_UNSET").is_none());
    }

    #[test]
    fn the_value_is_trimmed_and_never_printed() {
        std::env::set_var("CONVERSATION_TEST_CREDENTIAL_PRESENT", " not-a-real-value ");
        let credential = Credential::read("CONVERSATION_TEST_CREDENTIAL_PRESENT").unwrap();

        assert_eq!(credential.expose(), "not-a-real-value");
        assert_eq!(format!("{credential:?}"), "Credential(<redacted>)");
    }
}
