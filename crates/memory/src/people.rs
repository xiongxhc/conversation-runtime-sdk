use conversation_protocol::{PersonId, UnixTimestampMillis};

use crate::{
    MemoryStoreError, MemoryStoreErrorKind, MemoryStoreResult, SignatureId, SignatureKey,
    SignatureModel, VoiceSignature, VoiceSignatureDraft,
};

/// The most people one store holds, the owner included. Every spoken name claim can
/// enrol someone, so the bound keeps a room full of claims from growing storage forever.
pub const MAX_PEOPLE: usize = 256;

const MAX_NAME_BYTES: usize = 128;

/// A name as it is shown and the key it is compared by. The caller normalises the key
/// (pinyin for Chinese, case-folded for Latin); the store compares keys exactly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersonName {
    display: String,
    key: String,
}

impl PersonName {
    pub fn new(display: impl Into<String>, key: impl Into<String>) -> MemoryStoreResult<Self> {
        let display = display.into();
        let key = key.into();
        if !is_valid_name(&display) || !is_valid_name(&key) {
            return Err(MemoryStoreError::new(
                MemoryStoreErrorKind::Conflict,
                "person name must be 1 through 128 bytes and not blank",
            ));
        }
        Ok(Self { display, key })
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    pub fn key(&self) -> &str {
        &self.key
    }
}

fn is_valid_name(value: &str) -> bool {
    value.len() <= MAX_NAME_BYTES && !value.trim().is_empty()
}

/// A provisional person has a signature from too little speech to deny someone else's
/// claim; a mature one has enough.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PersonMaturity {
    Provisional,
    Mature,
}

impl PersonMaturity {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Mature => "mature",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Person {
    id: PersonId,
    is_owner: bool,
    names: Vec<PersonName>,
    maturity: PersonMaturity,
    needs_review: bool,
    created_at: UnixTimestampMillis,
}

impl Person {
    pub(crate) const fn new(
        id: PersonId,
        is_owner: bool,
        names: Vec<PersonName>,
        maturity: PersonMaturity,
        needs_review: bool,
        created_at: UnixTimestampMillis,
    ) -> Self {
        Self {
            id,
            is_owner,
            names,
            maturity,
            needs_review,
            created_at,
        }
    }

    pub const fn id(&self) -> PersonId {
        self.id
    }

    pub const fn is_owner(&self) -> bool {
        self.is_owner
    }

    /// The primary name first, then aliases. Empty only for an owner who has not set a
    /// name yet.
    pub fn names(&self) -> &[PersonName] {
        &self.names
    }

    pub fn display_name(&self) -> Option<&str> {
        self.names.first().map(PersonName::display)
    }

    pub const fn maturity(&self) -> PersonMaturity {
        self.maturity
    }

    /// Set on a person created as a possible duplicate, for the owner to merge or rename.
    pub const fn needs_review(&self) -> bool {
        self.needs_review
    }

    pub const fn created_at(&self) -> UnixTimestampMillis {
        self.created_at
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NewPerson {
    name: PersonName,
    needs_review: bool,
    created_at: UnixTimestampMillis,
}

impl NewPerson {
    pub const fn new(
        name: PersonName,
        needs_review: bool,
        created_at: UnixTimestampMillis,
    ) -> Self {
        Self {
            name,
            needs_review,
            created_at,
        }
    }

    pub(crate) const fn name(&self) -> &PersonName {
        &self.name
    }

    pub(crate) const fn needs_review(&self) -> bool {
        self.needs_review
    }

    pub(crate) const fn created_at(&self) -> UnixTimestampMillis {
        self.created_at
    }
}

/// People known to the runtime. Only the deployment's owner manages them, through the
/// SDK; nothing said in a conversation reaches these operations directly.
pub trait PeopleStore: Send + Sync {
    /// Everyone, ordered by identifier, so the owner comes first.
    fn people(&self) -> MemoryStoreResult<Vec<Person>>;

    fn person(&self, id: PersonId) -> MemoryStoreResult<Person>;

    /// Everyone with this name key as a name or an alias, ordered by identifier.
    fn people_named(&self, key: &str) -> MemoryStoreResult<Vec<PersonId>>;

    /// Creates a provisional person who is never the owner.
    fn create_person(&self, person: NewPerson) -> MemoryStoreResult<Person>;

    /// Adds an alias after the person's existing names.
    fn add_name(&self, id: PersonId, name: PersonName) -> MemoryStoreResult<Person>;

    /// Replaces the primary name, dropping an alias with the same key, and clears the
    /// review flag. For an owner without a name it sets the first one.
    fn rename(&self, id: PersonId, name: PersonName) -> MemoryStoreResult<Person>;

    fn mark_mature(&self, id: PersonId) -> MemoryStoreResult<Person>;

    /// Deletes the person with their names, voice signatures and every memory about them.
    fn delete_person(&self, id: PersonId) -> MemoryStoreResult<()>;

    /// Moves `from`'s memories, voice signatures and names onto `into`, then deletes `from`.
    fn merge_people(&self, from: PersonId, into: PersonId) -> MemoryStoreResult<Person>;

    /// Checks the voice signature key, recording it on first use. A different key later
    /// is `KeyMismatch`.
    fn verify_signature_key(&self, key: &SignatureKey) -> MemoryStoreResult<()>;

    fn add_signature(
        &self,
        key: &SignatureKey,
        person: PersonId,
        draft: VoiceSignatureDraft,
    ) -> MemoryStoreResult<VoiceSignature>;

    /// Every person's signatures from this model and version, ordered by person then
    /// identifier.
    fn signatures(
        &self,
        key: &SignatureKey,
        model: &SignatureModel,
    ) -> MemoryStoreResult<Vec<VoiceSignature>>;

    /// Removes a refinement sample. Anchor and enrolment samples are `Conflict`.
    fn remove_signature(&self, id: SignatureId) -> MemoryStoreResult<()>;
}
