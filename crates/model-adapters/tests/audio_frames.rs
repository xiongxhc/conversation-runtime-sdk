use conversation_model_adapters::{AudioFrame, PcmFormat, PcmSampleFormat};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};

#[test]
fn pcm_frame_requires_aligned_bounded_payload() {
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();

    assert!(AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        vec![0; 960],
    )
    .is_ok());
    assert!(AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        vec![0; 959],
    )
    .is_err());
    assert!(AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        0,
        format,
        vec![0; 65_536],
    )
    .is_ok());
}

#[test]
fn pcm_frame_alignment_covers_float_and_stereo_formats() {
    for (sample_format, channels, aligned, misaligned) in [
        (PcmSampleFormat::Signed16LittleEndian, 2, 4, 2),
        (PcmSampleFormat::Float32LittleEndian, 1, 4, 2),
        (PcmSampleFormat::Float32LittleEndian, 2, 8, 4),
    ] {
        let format = PcmFormat::new(24_000, channels, sample_format).unwrap();

        assert!(frame(0, format, vec![0; aligned]).is_ok());
        assert!(frame(0, format, vec![0; misaligned]).is_err());
    }
}

#[test]
fn pcm_format_requires_non_zero_sample_rate_and_channels() {
    assert!(PcmFormat::new(0, 1, PcmSampleFormat::Signed16LittleEndian).is_err());
    assert!(PcmFormat::new(24_000, 0, PcmSampleFormat::Signed16LittleEndian).is_err());
}

#[test]
fn pcm_frame_rejects_empty_and_oversized_payloads() {
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();

    assert!(frame(0, format, vec![]).is_err());
    assert!(frame(0, format, vec![0; 65_538]).is_err());
}

#[test]
fn pcm_frame_exposes_checked_next_sequence() {
    let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();

    assert_eq!(
        frame(7, format, vec![0; 2])
            .unwrap()
            .next_sequence()
            .unwrap(),
        8
    );
    assert!(frame(u64::MAX, format, vec![0; 2])
        .unwrap()
        .next_sequence()
        .is_err());
}

fn frame(
    sequence: u64,
    format: PcmFormat,
    bytes: Vec<u8>,
) -> Result<AudioFrame, conversation_model_adapters::AdapterError> {
    AudioFrame::new(
        TurnId::new(1),
        GenerationId::new(1),
        UtteranceId::new(1),
        sequence,
        format,
        bytes,
    )
}
