//! Scores a turn's voice embedding against everyone's stored signatures.

use std::collections::BTreeMap;

use conversation_memory::{SignatureRole, VoiceSignature, MAX_SIGNATURES_PER_PERSON};
use conversation_protocol::{PersonId, RuntimeError, RuntimeErrorKind, RuntimeStage};

/// Every value comes from measurement on real recordings; there are deliberately no
/// defaults. Scores are cosine similarities.
#[derive(Clone, Debug, PartialEq)]
pub struct IdentityPolicy {
    /// The best match is confident at or above this score, if it also leads by `margin`.
    pub confident_score: f32,
    /// Short of confident, a best match at or above this score is possible.
    pub possible_score: f32,
    /// How far the best match must lead the runner-up to be confident.
    pub margin: f32,
    /// The stricter score a turn needs against a new person's claim utterance to be
    /// collected as enrolment speech.
    pub anchor_score: f32,
    /// The shortest single-segment turn that may become a sample.
    pub min_sample_ms: u32,
    /// The stored speech at which a provisional person becomes mature.
    pub maturity_speech_ms: u32,
    /// The most refinement samples added for one person.
    pub refinement_cap: usize,
}

impl IdentityPolicy {
    pub fn validate(&self) -> Result<(), RuntimeError> {
        let scores_ordered = 0.0 < self.possible_score
            && self.possible_score <= self.confident_score
            && self.confident_score <= self.anchor_score
            && self.anchor_score <= 1.0;
        if !scores_ordered || !(0.0..=1.0).contains(&self.margin) {
            return Err(policy_error(
                "speaker identity scores must satisfy 0 < possible <= confident <= anchor <= 1 and a margin within 0 through 1",
            ));
        }
        if self.min_sample_ms < 1_500 || self.maturity_speech_ms < self.min_sample_ms {
            return Err(policy_error(
                "speaker identity samples must be at least 1500 ms and maturity at least one sample",
            ));
        }
        if self.refinement_cap > MAX_SIGNATURES_PER_PERSON {
            return Err(policy_error(
                "speaker identity refinement cap exceeds the signatures a person can hold",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum VoiceEvidence {
    Confident { person: PersonId, score: f32 },
    Possible { person: PersonId, score: f32 },
    Unknown,
}

struct Entry {
    centroid: Vec<f32>,
    refinements: usize,
    speech_ms: u64,
    anchor: Option<Vec<f32>>,
}

/// Everyone's signatures from one model, reduced to one centroid per person.
pub struct VoiceDirectory {
    dimension: Option<usize>,
    people: BTreeMap<PersonId, Entry>,
}

impl VoiceDirectory {
    /// Refuses signatures of different sizes: they cannot have come from one model.
    pub fn new(signatures: &[VoiceSignature]) -> Result<Self, RuntimeError> {
        let dimension = signatures
            .first()
            .map(|signature| signature.embedding().len());
        let mut grouped: BTreeMap<PersonId, Vec<&VoiceSignature>> = BTreeMap::new();
        for signature in signatures {
            if Some(signature.embedding().len()) != dimension {
                return Err(policy_error(
                    "stored voice signatures for one model differ in size",
                ));
            }
            grouped
                .entry(signature.person())
                .or_default()
                .push(signature);
        }
        let people = grouped
            .into_iter()
            .map(|(person, samples)| {
                let mut sum = vec![0.0; dimension.unwrap_or(0)];
                for sample in &samples {
                    for (total, value) in sum.iter_mut().zip(normalized(sample.embedding())) {
                        *total += value;
                    }
                }
                let entry = Entry {
                    centroid: normalized(&sum),
                    refinements: samples
                        .iter()
                        .filter(|sample| sample.role() == SignatureRole::Refinement)
                        .count(),
                    speech_ms: samples
                        .iter()
                        .map(|sample| u64::from(sample.speech_ms()))
                        .sum(),
                    anchor: samples
                        .iter()
                        .find(|sample| sample.role() == SignatureRole::Anchor)
                        .map(|sample| sample.embedding().to_vec()),
                };
                (person, entry)
            })
            .collect();
        Ok(Self { dimension, people })
    }

    pub const fn dimension(&self) -> Option<usize> {
        self.dimension
    }

    /// Everyone's score, best first, ties by person. `None` when the directory is empty or
    /// the embedding has another size.
    pub fn scores(&self, embedding: &[f32]) -> Option<Vec<(PersonId, f32)>> {
        if self.dimension != Some(embedding.len()) {
            return None;
        }
        let mut scores: Vec<(PersonId, f32)> = self
            .people
            .iter()
            .map(|(person, entry)| (*person, cosine(embedding, &entry.centroid)))
            .collect();
        scores.sort_by(|left, right| right.1.total_cmp(&left.1).then(left.0.cmp(&right.0)));
        Some(scores)
    }

    pub fn refinements(&self, person: PersonId) -> usize {
        self.people
            .get(&person)
            .map_or(0, |entry| entry.refinements)
    }

    pub fn speech_ms(&self, person: PersonId) -> u64 {
        self.people.get(&person).map_or(0, |entry| entry.speech_ms)
    }

    pub fn anchor(&self, person: PersonId) -> Option<&[f32]> {
        self.people
            .get(&person)
            .and_then(|entry| entry.anchor.as_deref())
    }
}

pub fn cosine(left: &[f32], right: &[f32]) -> f32 {
    let dot: f32 = left.iter().zip(right).map(|(a, b)| a * b).sum();
    let norms = norm(left) * norm(right);
    if norms == 0.0 {
        0.0
    } else {
        dot / norms
    }
}

/// Confident needs both the score and the lead over the runner-up; a lone known person is
/// measured against zero.
pub fn classify(scores: &[(PersonId, f32)], policy: &IdentityPolicy) -> VoiceEvidence {
    let Some(&(person, score)) = scores.first() else {
        return VoiceEvidence::Unknown;
    };
    let runner_up = scores.get(1).map_or(0.0, |(_, score)| *score);
    if score >= policy.confident_score && score - runner_up >= policy.margin {
        VoiceEvidence::Confident { person, score }
    } else if score >= policy.possible_score {
        VoiceEvidence::Possible { person, score }
    } else {
        VoiceEvidence::Unknown
    }
}

fn norm(values: &[f32]) -> f32 {
    values.iter().map(|value| value * value).sum::<f32>().sqrt()
}

fn normalized(values: &[f32]) -> Vec<f32> {
    let length = norm(values);
    if length == 0.0 {
        values.to_vec()
    } else {
        values.iter().map(|value| value / length).collect()
    }
}

fn policy_error(message: &'static str) -> RuntimeError {
    RuntimeError::new(
        RuntimeErrorKind::Configuration,
        RuntimeStage::Memory,
        message,
    )
}
