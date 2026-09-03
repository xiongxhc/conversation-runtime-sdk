//! The provider-neutral batch transcription and diarization port.
//!
//! A diarizer is handed one *bounded window* of decrypted meeting audio at a time and answers
//! with timed text and anonymous speaker clusters. The port is deliberately narrow:
//!
//! - It never sees a file, a path, a recording handle or a key. Whoever owns the recording
//!   decrypts it in memory and pushes windows down a channel, so plaintext meeting audio never
//!   reaches disk on the way to a provider.
//! - It never emits a person. A provider's own cluster label is discarded and replaced by an
//!   adapter-minted `cluster-N`, so an identity a provider guessed cannot travel out of the
//!   adapter. Mapping a cluster onto a real participant is the Meeting Agent's job, from
//!   evidence this runtime does not have.
//!
//! # The clock
//!
//! Every timestamp on this port — a window's `started_at_ms` and a segment's `started_at_ms`
//! and `ended_at_ms` — is on the **media timeline**: the same origin and the same units as the
//! `duration_ms` and the gap timestamps of a finalized recording summary. It is *not* the
//! recorder's wall clock, which is what the timestamps inside the encrypted file are on, and it
//! is *not* an audio-only offset: a gap between two frames occupies its real length on this
//! timeline even though no audio was recorded for it.

use std::fmt;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture, PcmFormat};

/// The most segments one diarization may carry. A longer meeting fails closed rather than
/// producing a result that cannot be stated on a bounded wire frame: 1024 segments of
/// timings, identifier and cluster label cost about 133 KiB of JSON, which together with the
/// text limit below leaves the whole answer inside the 512 KiB client frame even when every
/// character of text has to be escaped.
pub const MAX_DIARIZED_SEGMENTS: usize = 1024;
/// The most text one segment may carry.
pub const MAX_DIARIZED_SEGMENT_TEXT_BYTES: usize = 2048;
/// The most text a whole diarization may carry, across every segment.
pub const MAX_DIARIZED_TEXT_BYTES: usize = 128 * 1024;
/// The most distinct speaker clusters one diarization may carry.
pub const MAX_SPEAKER_CLUSTERS: usize = 64;
/// The most bytes one segment identifier may carry.
pub const MAX_DIARIZED_SEGMENT_ID_BYTES: usize = 64;

/// A bounded window of decrypted meeting audio.
///
/// `started_at_ms` is the window's first sample on the media timeline. Windows arrive in order
/// and never overlap; consecutive windows are contiguous *except* across a recording gap, where
/// the next window starts later than the previous one ended.
#[derive(Clone, Eq, PartialEq)]
pub struct MeetingAudioWindow {
    format: PcmFormat,
    started_at_ms: u64,
    bytes: Vec<u8>,
}

/// Deliberately not derived: a window is plaintext meeting audio, and a derived `Debug` would
/// put it in any log line that prints the value.
impl fmt::Debug for MeetingAudioWindow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MeetingAudioWindow")
            .field("format", &self.format)
            .field("started_at_ms", &self.started_at_ms)
            .field("bytes", &format_args!("<{} bytes>", self.bytes.len()))
            .finish()
    }
}

impl MeetingAudioWindow {
    pub fn new(
        format: PcmFormat,
        started_at_ms: u64,
        bytes: Vec<u8>,
    ) -> Result<Self, AdapterError> {
        if bytes.is_empty() {
            return Err(AdapterError::new("meeting audio window must not be empty"));
        }
        if !bytes.len().is_multiple_of(format.frame_alignment_bytes()?) {
            return Err(AdapterError::new(
                "meeting audio window was not sample aligned",
            ));
        }
        Ok(Self {
            format,
            started_at_ms,
            bytes,
        })
    }

    pub const fn format(&self) -> PcmFormat {
        self.format
    }

    /// The window's first sample on the media timeline.
    pub const fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// How much audio this window carries, which is *not* how much of the media timeline it
    /// covers when a gap follows it.
    pub fn audio_ms(&self) -> u64 {
        let Ok(alignment) = self.format.frame_alignment_bytes() else {
            return 0;
        };
        if alignment == 0 {
            return 0;
        }
        let samples = self.bytes.len() / alignment;
        u64::try_from(samples as u128 * 1000 / u128::from(self.format.sample_rate_hz()))
            .unwrap_or(u64::MAX)
    }
}

/// The channel a recording owner pushes windows down. It closes when the recording is
/// exhausted, and carries an error when the recording could not be read further.
pub type MeetingAudioWindows = mpsc::Receiver<Result<MeetingAudioWindow, AdapterError>>;

/// One stretch of speech, with its cluster.
///
/// `started_at_ms` and `ended_at_ms` are on the **media timeline** (see the module comment).
/// `speaker_cluster` is an anonymous adapter-minted label such as `cluster-1`; it never names a
/// person, and two segments carrying one label only claim the provider heard one voice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiarizedSegment {
    segment_id: String,
    started_at_ms: u64,
    ended_at_ms: u64,
    text: String,
    speaker_cluster: Option<String>,
}

impl DiarizedSegment {
    pub fn new(
        segment_id: impl Into<String>,
        started_at_ms: u64,
        ended_at_ms: u64,
        text: impl Into<String>,
        speaker_cluster: Option<String>,
    ) -> Result<Self, AdapterError> {
        let segment_id = segment_id.into();
        let text = text.into();
        if segment_id.is_empty()
            || segment_id.len() > MAX_DIARIZED_SEGMENT_ID_BYTES
            || !segment_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err(AdapterError::new(
                "diarized segment identifier is not a bounded lowercase label",
            ));
        }
        if ended_at_ms < started_at_ms {
            return Err(AdapterError::new("diarized segment ends before it starts"));
        }
        if text.len() > MAX_DIARIZED_SEGMENT_TEXT_BYTES {
            return Err(AdapterError::new(
                "diarized segment text exceeded its size limit",
            ));
        }
        // A control character would be escaped six-fold on the wire, so refusing it is what
        // keeps the published byte budget above honest.
        if text.chars().any(char::is_control) {
            return Err(AdapterError::new(
                "diarized segment text carries a control character",
            ));
        }
        if let Some(cluster) = speaker_cluster.as_deref() {
            if !is_anonymous_cluster_label(cluster) {
                return Err(AdapterError::new(
                    "diarized segment speaker cluster is not an anonymous label",
                ));
            }
        }
        Ok(Self {
            segment_id,
            started_at_ms,
            ended_at_ms,
            text,
            speaker_cluster,
        })
    }

    pub fn segment_id(&self) -> &str {
        &self.segment_id
    }

    pub const fn started_at_ms(&self) -> u64 {
        self.started_at_ms
    }

    pub const fn ended_at_ms(&self) -> u64 {
        self.ended_at_ms
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn speaker_cluster(&self) -> Option<&str> {
        self.speaker_cluster.as_deref()
    }
}

/// Why a diarization is less than a perfect account of the meeting.
///
/// A closed list, exactly as the recording gap reasons are: a provider's own warning text is
/// unbounded and may quote speech, so it is counted rather than relayed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiarizationWarning {
    /// The provider returned no speech at all.
    NoSpeechDetected,
    /// The provider reported warnings of its own, which are not relayed.
    ProviderReportedWarnings,
    /// The recording's tail was torn, which is what a crash mid-write leaves behind.
    RecordingTruncated,
}

impl DiarizationWarning {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoSpeechDetected => "no_speech_detected",
            Self::ProviderReportedWarnings => "provider_reported_warnings",
            Self::RecordingTruncated => "recording_truncated",
        }
    }
}

/// A whole meeting's diarization.
///
/// Constructed only through [`DiarizationResult::new`], which is where the maximum result size
/// is enforced: a provider that answers with more than the runtime can state fails the request
/// instead of having its answer silently truncated.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiarizationResult {
    segments: Vec<DiarizedSegment>,
    warnings: Vec<DiarizationWarning>,
}

impl DiarizationResult {
    pub fn new(
        segments: Vec<DiarizedSegment>,
        warnings: Vec<DiarizationWarning>,
    ) -> Result<Self, AdapterError> {
        if segments.len() > MAX_DIARIZED_SEGMENTS {
            return Err(AdapterError::new("diarization exceeded its segment limit"));
        }
        let mut used_bytes = 0_usize;
        let mut clusters = Vec::new();
        for (index, segment) in segments.iter().enumerate() {
            used_bytes = used_bytes
                .checked_add(segment.text.len())
                .filter(|used| *used <= MAX_DIARIZED_TEXT_BYTES)
                .ok_or_else(|| AdapterError::new("diarization exceeded its text limit"))?;
            if segments[..index]
                .iter()
                .any(|earlier| earlier.segment_id == segment.segment_id)
            {
                return Err(AdapterError::new(
                    "diarization repeated a segment identifier",
                ));
            }
            if let Some(cluster) = segment.speaker_cluster.as_deref() {
                if !clusters.iter().any(|known| known == cluster) {
                    clusters.push(cluster.to_owned());
                }
            }
        }
        if clusters.len() > MAX_SPEAKER_CLUSTERS {
            return Err(AdapterError::new(
                "diarization exceeded its speaker cluster limit",
            ));
        }
        let mut deduplicated = Vec::with_capacity(warnings.len());
        for warning in warnings {
            if !deduplicated.contains(&warning) {
                deduplicated.push(warning);
            }
        }
        Ok(Self {
            segments,
            warnings: deduplicated,
        })
    }

    pub fn segments(&self) -> &[DiarizedSegment] {
        &self.segments
    }

    pub fn warnings(&self) -> &[DiarizationWarning] {
        &self.warnings
    }

    /// Adds a warning the caller observed rather than the provider. Used by the recording owner
    /// for facts only it knows, such as a torn recording tail.
    pub fn with_warning(mut self, warning: DiarizationWarning) -> Self {
        if !self.warnings.contains(&warning) {
            self.warnings.push(warning);
        }
        self
    }

    pub fn into_parts(self) -> (Vec<DiarizedSegment>, Vec<DiarizationWarning>) {
        (self.segments, self.warnings)
    }
}

/// Batch transcription and diarization over a recording that has already stopped.
///
/// The implementation consumes `windows` in order and answers once. It must not buffer the
/// whole meeting: the window channel exists precisely so that a two-hour recording costs one
/// window of resident plaintext, not the whole file.
pub trait TranscriptionDiarizer: Send + Sync {
    fn diarize<'a>(
        &'a self,
        windows: MeetingAudioWindows,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, DiarizationResult>;
}

/// The anonymous labels an adapter hands out, in the order the provider first mentioned each
/// of its own clusters.
///
/// The provider's label never leaves this structure. That is what makes "the result never
/// claims a person's identity" a property of the code rather than a hope about the provider:
/// even a provider that answers `"speaker": "Chris Xiong"` produces `cluster-1`.
#[derive(Debug, Default)]
pub(crate) struct SpeakerClusters {
    provider_labels: Vec<String>,
}

impl SpeakerClusters {
    pub(crate) fn label_for(&mut self, provider_label: &str) -> Result<String, AdapterError> {
        let index = match self
            .provider_labels
            .iter()
            .position(|known| known == provider_label)
        {
            Some(index) => index,
            None => {
                if self.provider_labels.len() >= MAX_SPEAKER_CLUSTERS {
                    return Err(AdapterError::new(
                        "diarization exceeded its speaker cluster limit",
                    ));
                }
                self.provider_labels.push(provider_label.to_owned());
                self.provider_labels.len() - 1
            }
        };
        Ok(format!("cluster-{}", index + 1))
    }
}

/// The shape every emitted cluster label has: `cluster-` and a positive decimal. Anything else
/// — a provider's own label above all — is refused rather than passed on.
fn is_anonymous_cluster_label(label: &str) -> bool {
    let Some(index) = label.strip_prefix("cluster-") else {
        return false;
    };
    !index.is_empty()
        && index.len() <= 4
        && index.bytes().all(|byte| byte.is_ascii_digit())
        && !index.starts_with('0')
}

#[cfg(test)]
mod tests {
    use super::{
        is_anonymous_cluster_label, DiarizationResult, DiarizationWarning, DiarizedSegment,
        SpeakerClusters, MAX_DIARIZED_SEGMENTS, MAX_DIARIZED_SEGMENT_TEXT_BYTES,
    };

    fn segment(index: usize, text: &str) -> DiarizedSegment {
        DiarizedSegment::new(format!("seg-{index}"), 0, 1, text, None).unwrap()
    }

    #[test]
    fn a_segment_refuses_an_unbounded_identifier_and_control_characters() {
        assert!(DiarizedSegment::new("SEG-1", 0, 1, "text", None).is_err());
        assert!(DiarizedSegment::new("seg 1", 0, 1, "text", None).is_err());
        assert!(DiarizedSegment::new("s".repeat(65), 0, 1, "text", None).is_err());
        assert!(DiarizedSegment::new("seg-1", 0, 1, "line\nbreak", None).is_err());
    }

    #[test]
    fn a_provider_label_never_survives_relabelling() {
        let mut clusters = SpeakerClusters::default();

        assert_eq!(clusters.label_for("Chris Xiong").unwrap(), "cluster-1");
        assert_eq!(clusters.label_for("speaker_b").unwrap(), "cluster-2");
        assert_eq!(clusters.label_for("Chris Xiong").unwrap(), "cluster-1");
        assert!(is_anonymous_cluster_label("cluster-1"));
        assert!(!is_anonymous_cluster_label("Chris Xiong"));
        assert!(!is_anonymous_cluster_label("cluster-"));
        assert!(!is_anonymous_cluster_label("cluster-01"));
    }

    #[test]
    fn a_segment_refuses_a_provider_label_and_inverted_timings() {
        assert!(
            DiarizedSegment::new("seg-1", 0, 1, "text", Some("Chris Xiong".to_owned())).is_err()
        );
        assert!(DiarizedSegment::new("seg-1", 5, 4, "text", None).is_err());
        assert!(DiarizedSegment::new("", 0, 1, "text", None).is_err());
        assert!(DiarizedSegment::new(
            "seg-1",
            0,
            1,
            "x".repeat(MAX_DIARIZED_SEGMENT_TEXT_BYTES + 1),
            None
        )
        .is_err());
    }

    #[test]
    fn the_result_bounds_its_own_size() {
        let too_many = (0..=MAX_DIARIZED_SEGMENTS)
            .map(|index| segment(index, ""))
            .collect::<Vec<_>>();
        assert!(DiarizationResult::new(too_many, Vec::new()).is_err());

        let too_much_text = (0..128)
            .map(|index| segment(index, &"x".repeat(MAX_DIARIZED_SEGMENT_TEXT_BYTES)))
            .collect::<Vec<_>>();
        assert!(DiarizationResult::new(too_much_text, Vec::new()).is_err());

        assert!(
            DiarizationResult::new(vec![segment(1, "a"), segment(1, "b")], Vec::new()).is_err()
        );

        let result = DiarizationResult::new(
            vec![segment(1, "a")],
            vec![
                DiarizationWarning::NoSpeechDetected,
                DiarizationWarning::NoSpeechDetected,
            ],
        )
        .unwrap();
        assert_eq!(result.warnings(), [DiarizationWarning::NoSpeechDetected]);
    }
}
