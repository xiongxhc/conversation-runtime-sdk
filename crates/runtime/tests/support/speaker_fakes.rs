//! Fakes for the speaker identity stage's replaceable inputs, shared by the runtime tests.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use conversation_memory::{
    NewPerson, PeopleStore, PersonName, SignatureKey, SignatureModel, SignatureRole,
    SqliteMemoryStore, VoiceSignatureDraft,
};
use conversation_protocol::{PersonId, RuntimeError, SessionId, UnixTimestampMillis};
use conversation_runtime::{
    IdentityPolicy, NameClaimDetector, SpeakerFuture, SpeakerIdentityStage, TurnAudio,
    TurnAudioSource, VoiceEmbedder,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;

pub const CHRIS: [f32; 4] = [1.0, 0.0, 0.0, 0.0];
pub const JACK: [f32; 4] = [0.0, 1.0, 0.0, 0.0];
pub const NEW: [f32; 4] = [0.0, 0.0, 1.0, 0.0];

pub fn policy() -> IdentityPolicy {
    IdentityPolicy {
        confident_score: 0.80,
        possible_score: 0.60,
        margin: 0.10,
        anchor_score: 0.85,
        min_sample_ms: 1_500,
        maturity_speech_ms: 5_000,
        refinement_cap: 3,
    }
}

pub fn at() -> UnixTimestampMillis {
    UnixTimestampMillis::new(10_000).unwrap()
}

/// Audio whose first sample names the voice the fake embedder returns.
pub struct FakeAudio {
    voices: Mutex<std::collections::HashMap<u64, (f32, u32)>>,
    pub barge_in_seen: Mutex<Vec<u64>>,
}

impl FakeAudio {
    pub fn new() -> Self {
        Self {
            voices: Mutex::new(Default::default()),
            barge_in_seen: Mutex::new(Vec::new()),
        }
    }

    /// Segment `segment_id` sounds like `voice` (1 = Chris, 2 = Jack, 3 = new) for `speech_ms`.
    pub fn speak(&self, segment_id: u64, voice: f32, speech_ms: u32) {
        self.voices
            .lock()
            .unwrap()
            .insert(segment_id, (voice, speech_ms));
    }
}

impl TurnAudioSource for FakeAudio {
    fn turn_audio(
        &self,
        _session: SessionId,
        segment_id: u64,
        _cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Option<TurnAudio>> {
        let voice = self.voices.lock().unwrap().get(&segment_id).copied();
        Box::pin(async move {
            let (voice, speech_ms) = voice?;
            Some(TurnAudio {
                samples: vec![voice; (speech_ms as usize) * 16],
                playback_overlap: false,
                single_segment: true,
            })
        })
    }
}

pub struct FakeEmbedder {
    model: SignatureModel,
    pub delay: Mutex<Option<Duration>>,
    pub fail: AtomicBool,
    pub tokens: Mutex<Vec<CancellationToken>>,
}

impl FakeEmbedder {
    pub fn new() -> Self {
        Self {
            model: SignatureModel::new("ecapa", "1").unwrap(),
            delay: Mutex::new(None),
            fail: AtomicBool::new(false),
            tokens: Mutex::new(Vec::new()),
        }
    }
}

impl VoiceEmbedder for FakeEmbedder {
    fn model(&self) -> &SignatureModel {
        &self.model
    }

    fn embed(
        &self,
        samples: Vec<f32>,
        cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Vec<f32>, RuntimeError>> {
        self.tokens.lock().unwrap().push(cancellation.clone());
        let delay = *self.delay.lock().unwrap();
        let fail = self.fail.load(Ordering::SeqCst);
        Box::pin(async move {
            if let Some(delay) = delay {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = cancellation.cancelled() => {}
                }
            }
            if fail {
                return Err(RuntimeError::new(
                    conversation_protocol::RuntimeErrorKind::Adapter,
                    conversation_protocol::RuntimeStage::Memory,
                    "fake embedder failed",
                ));
            }
            let voice = samples.first().copied().unwrap_or_default();
            Ok(if voice == 1.0 {
                CHRIS.to_vec()
            } else if voice == 2.0 {
                JACK.to_vec()
            } else {
                NEW.to_vec()
            })
        })
    }
}

pub struct FakeClaims {
    pub name: Mutex<Option<String>>,
    pub calls: AtomicUsize,
    pub fail: AtomicBool,
    /// Cancelled when the detector is called, to land a cancellation right after gathering.
    pub cancel_on_call: Mutex<Option<CancellationToken>>,
}

impl FakeClaims {
    pub fn new() -> Self {
        Self {
            name: Mutex::new(None),
            calls: AtomicUsize::new(0),
            fail: AtomicBool::new(false),
            cancel_on_call: Mutex::new(None),
        }
    }
}

impl NameClaimDetector for FakeClaims {
    fn claimed_name(
        &self,
        _transcript: String,
        _cancellation: CancellationToken,
    ) -> SpeakerFuture<'_, Result<Option<String>, RuntimeError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(token) = self.cancel_on_call.lock().unwrap().as_ref() {
            token.cancel();
        }
        let name = self.name.lock().unwrap().clone();
        let fail = self.fail.load(Ordering::SeqCst);
        Box::pin(async move {
            if fail {
                return Err(RuntimeError::new(
                    conversation_protocol::RuntimeErrorKind::Adapter,
                    conversation_protocol::RuntimeStage::LanguageModel,
                    "fake detector failed",
                ));
            }
            Ok(name)
        })
    }
}

pub struct StageFixture {
    _temporary: TempDir,
    pub store: Arc<SqliteMemoryStore>,
    pub key: Arc<SignatureKey>,
    pub audio: Arc<FakeAudio>,
    pub embedder: Arc<FakeEmbedder>,
    pub claims: Arc<FakeClaims>,
}

impl StageFixture {
    pub fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteMemoryStore::initialize(temporary.path().join("runtime.sqlite3")).unwrap(),
        );
        Self {
            _temporary: temporary,
            store,
            key: Arc::new(SignatureKey::from_hex(&"11".repeat(32)).unwrap()),
            audio: Arc::new(FakeAudio::new()),
            embedder: Arc::new(FakeEmbedder::new()),
            claims: Arc::new(FakeClaims::new()),
        }
    }

    pub fn stage(&self, wait: Duration) -> SpeakerIdentityStage {
        SpeakerIdentityStage::new(
            self.store.clone(),
            self.key.clone(),
            policy(),
            wait,
            self.audio.clone(),
            self.embedder.clone(),
            self.claims.clone(),
        )
    }

    /// A mature person with a stored voice.
    pub fn person(&self, name: &str, voice: &[f32]) -> PersonId {
        let id = self
            .store
            .create_person(NewPerson::new(
                PersonName::new(name, name.to_lowercase()).unwrap(),
                false,
                at(),
            ))
            .unwrap()
            .id();
        for (role, speech_ms) in [
            (SignatureRole::Anchor, 1_000),
            (SignatureRole::Enrolment, 5_000),
        ] {
            self.store
                .add_signature(
                    &self.key,
                    id,
                    VoiceSignatureDraft::new(
                        SignatureModel::new("ecapa", "1").unwrap(),
                        role,
                        voice.to_vec(),
                        speech_ms,
                        at(),
                        SessionId::new(1),
                    )
                    .unwrap(),
                )
                .unwrap();
        }
        self.store.mark_mature(id).unwrap();
        id
    }
}
