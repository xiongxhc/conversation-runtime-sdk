use std::fmt;

use conversation_protocol::{PersonId, SessionId, UnixTimestampMillis};
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_256_GCM, NONCE_LEN};
use ring::digest::{digest, SHA256};
use ring::rand::{SecureRandom, SystemRandom};

use crate::{MemoryStoreError, MemoryStoreErrorKind, MemoryStoreResult};

/// The most voice signatures one person holds. Enrolment and refinement both add samples,
/// so the bound keeps one voice from growing storage forever.
pub const MAX_SIGNATURES_PER_PERSON: usize = 64;

const KEY_HEX_LEN: usize = 64;
const MAX_KEY_ENV_BYTES: usize = 128;
const MAX_MODEL_FIELD_BYTES: usize = 128;
const MAX_EMBEDDING_DIMENSION: usize = 4_096;
const KEY_DOMAIN: &[u8] = b"conversation-runtime/voice-signature/key/v1\0";
const SIGNATURE_DOMAIN: &[u8] = b"conversation-runtime/voice-signature/v1\0";
pub(crate) const KEY_CHECK_PLAINTEXT: &[u8] = b"conversation-runtime/voice-signature-key/v1";
pub(crate) const KEY_CHECK_AAD: &[u8] = b"key-check";

/// The key voice signatures are encrypted with. Its material follows the recording key's
/// format, a 256-bit key written as 64 hexadecimal characters, and is domain-separated, so
/// the same value used for recordings yields a different key here. The key is never
/// printed, stored, or formatted into an error.
pub struct SignatureKey {
    key: LessSafeKey,
}

impl SignatureKey {
    pub fn from_hex(material: &str) -> MemoryStoreResult<Self> {
        let material = material.trim().to_ascii_lowercase();
        if material.len() != KEY_HEX_LEN || !material.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(key_unavailable_error());
        }
        let mut input = Vec::with_capacity(KEY_DOMAIN.len() + material.len());
        input.extend_from_slice(KEY_DOMAIN);
        input.extend_from_slice(material.as_bytes());
        let derived = digest(&SHA256, &input);
        let key = UnboundKey::new(&AES_256_GCM, derived.as_ref())
            .map(LessSafeKey::new)
            .map_err(|_| key_unavailable_error())?;
        Ok(Self { key })
    }

    /// Reads the key from the named environment variable. A missing variable, an invalid
    /// name, or malformed material are all `KeyUnavailable`.
    pub fn from_environment(name: &str) -> MemoryStoreResult<Self> {
        if !is_environment_name(name) {
            return Err(key_unavailable_error());
        }
        let material = std::env::var(name).map_err(|_| key_unavailable_error())?;
        Self::from_hex(&material)
    }

    pub(crate) fn seal(
        &self,
        aad: &[u8],
        plaintext: &[u8],
    ) -> MemoryStoreResult<([u8; NONCE_LEN], Vec<u8>)> {
        let mut nonce = [0_u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| sealing_error())?;
        let mut sealed = plaintext.to_vec();
        self.key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut sealed,
            )
            .map_err(|_| sealing_error())?;
        Ok((nonce, sealed))
    }

    /// Returns `None` when the ciphertext does not authenticate under this key and AAD.
    pub(crate) fn open(
        &self,
        nonce: [u8; NONCE_LEN],
        aad: &[u8],
        ciphertext: &[u8],
    ) -> Option<Vec<u8>> {
        let mut opened = ciphertext.to_vec();
        let length = self
            .key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad),
                &mut opened,
            )
            .ok()?
            .len();
        opened.truncate(length);
        Some(opened)
    }
}

impl fmt::Debug for SignatureKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SignatureKey(..)")
    }
}

fn is_environment_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_KEY_ENV_BYTES
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

/// Which embedding model produced a signature. Signatures from different models, or
/// different versions of one model, are never compared.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SignatureModel {
    id: String,
    version: String,
}

impl SignatureModel {
    pub fn new(id: impl Into<String>, version: impl Into<String>) -> MemoryStoreResult<Self> {
        let id = id.into();
        let version = version.into();
        let valid = |value: &str| !value.is_empty() && value.len() <= MAX_MODEL_FIELD_BYTES;
        if !valid(&id) || !valid(&version) {
            return Err(MemoryStoreError::new(
                MemoryStoreErrorKind::Conflict,
                "signature model identifier and version must be 1 through 128 bytes",
            ));
        }
        Ok(Self { id, version })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

/// The anchor is the utterance a person named themselves in; enrolment samples are the
/// ones that made the person mature. Both are immutable. Refinement samples come from later
/// confident matches and are the only ones that can be removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignatureRole {
    Anchor,
    Enrolment,
    Refinement,
}

impl SignatureRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Anchor => "anchor",
            Self::Enrolment => "enrolment",
            Self::Refinement => "refinement",
        }
    }

    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "anchor" => Some(Self::Anchor),
            "enrolment" => Some(Self::Enrolment),
            "refinement" => Some(Self::Refinement),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceSignatureDraft {
    model: SignatureModel,
    role: SignatureRole,
    embedding: Vec<f32>,
    speech_ms: u32,
    created_at: UnixTimestampMillis,
    source_session: SessionId,
}

impl VoiceSignatureDraft {
    pub fn new(
        model: SignatureModel,
        role: SignatureRole,
        embedding: Vec<f32>,
        speech_ms: u32,
        created_at: UnixTimestampMillis,
        source_session: SessionId,
    ) -> MemoryStoreResult<Self> {
        if embedding.is_empty()
            || embedding.len() > MAX_EMBEDDING_DIMENSION
            || !embedding.iter().all(|value| value.is_finite())
        {
            return Err(MemoryStoreError::new(
                MemoryStoreErrorKind::Conflict,
                "voice signature must hold 1 through 4096 finite values",
            ));
        }
        Ok(Self {
            model,
            role,
            embedding,
            speech_ms,
            created_at,
            source_session,
        })
    }

    pub(crate) const fn model(&self) -> &SignatureModel {
        &self.model
    }

    pub(crate) const fn role(&self) -> SignatureRole {
        self.role
    }

    pub(crate) fn embedding(&self) -> &[f32] {
        &self.embedding
    }

    pub(crate) const fn speech_ms(&self) -> u32 {
        self.speech_ms
    }

    pub(crate) const fn created_at(&self) -> UnixTimestampMillis {
        self.created_at
    }

    pub(crate) const fn source_session(&self) -> SessionId {
        self.source_session
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SignatureId(u64);

impl SignatureId {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceSignature {
    id: SignatureId,
    person: PersonId,
    draft: VoiceSignatureDraft,
}

impl VoiceSignature {
    pub(crate) const fn new(id: SignatureId, person: PersonId, draft: VoiceSignatureDraft) -> Self {
        Self { id, person, draft }
    }

    pub const fn id(&self) -> SignatureId {
        self.id
    }

    pub const fn person(&self) -> PersonId {
        self.person
    }

    pub const fn model(&self) -> &SignatureModel {
        &self.draft.model
    }

    pub const fn role(&self) -> SignatureRole {
        self.draft.role
    }

    pub fn embedding(&self) -> &[f32] {
        &self.draft.embedding
    }

    pub const fn speech_ms(&self) -> u32 {
        self.draft.speech_ms
    }

    pub const fn created_at(&self) -> UnixTimestampMillis {
        self.draft.created_at
    }

    pub const fn source_session(&self) -> SessionId {
        self.draft.source_session
    }
}

/// Binds a signature's ciphertext to its model, role and size, so a row whose metadata
/// was changed no longer decrypts.
pub(crate) fn signature_aad(
    model: &SignatureModel,
    role: SignatureRole,
    dimension: u32,
) -> Vec<u8> {
    let mut aad =
        Vec::with_capacity(SIGNATURE_DOMAIN.len() + model.id.len() + model.version.len() + 32);
    aad.extend_from_slice(SIGNATURE_DOMAIN);
    aad.extend_from_slice(model.id.as_bytes());
    aad.push(0);
    aad.extend_from_slice(model.version.as_bytes());
    aad.push(0);
    aad.extend_from_slice(role.as_str().as_bytes());
    aad.push(0);
    aad.extend_from_slice(&dimension.to_le_bytes());
    aad
}

pub(crate) const fn key_mismatch_error() -> MemoryStoreError {
    MemoryStoreError::new(
        MemoryStoreErrorKind::KeyMismatch,
        "voice signature key does not match the key this store was first used with",
    )
}

const fn key_unavailable_error() -> MemoryStoreError {
    MemoryStoreError::new(
        MemoryStoreErrorKind::KeyUnavailable,
        "voice signature key is unavailable or not 64 hexadecimal characters",
    )
}

const fn sealing_error() -> MemoryStoreError {
    MemoryStoreError::new(
        MemoryStoreErrorKind::Storage,
        "voice signature could not be encrypted",
    )
}
