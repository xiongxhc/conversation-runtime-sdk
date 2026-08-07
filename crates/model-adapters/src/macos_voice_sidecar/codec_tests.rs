use std::fs;
use std::path::{Path, PathBuf};

use super::codec::{
    decode_audio_payload, decode_frame, decode_frame_at_eof, encode_frame, SidecarCodecError,
    SidecarControl, SidecarFailureCode, SidecarFrame, SidecarFrameKind, AUDIO_METADATA_BYTES,
    HEADER_BYTES, MAX_AUDIO_PAYLOAD_BYTES, MAX_CONTROL_PAYLOAD_BYTES, PROTOCOL_VERSION,
};
use crate::{AudioFrame, PcmFormat, PcmSampleFormat, RecognitionHypothesis};
use conversation_protocol::{
    GenerationId, RuntimeStage, SessionId, TurnId, UtteranceId, VoiceActivity,
};

const FIXTURE_ROOT: &str = "../../tests/fixtures/voice-sidecar-v1";

#[test]
fn version_one_capture_kind_codes_are_pinned() {
    assert_eq!(PROTOCOL_VERSION, 1);
    let expected = [
        (SidecarFrameKind::StartSession, 0x0001),
        (SidecarFrameKind::StartCapture, 0x0002),
        (SidecarFrameKind::FlushGeneration, 0x0003),
        (SidecarFrameKind::Shutdown, 0x0004),
        (SidecarFrameKind::PauseCapture, 0x0005),
        (SidecarFrameKind::ResumeCapture, 0x0006),
        (SidecarFrameKind::AudioFrame, 0x0100),
        (SidecarFrameKind::Ready, 0x8001),
        (SidecarFrameKind::VoiceActivity, 0x8002),
        (SidecarFrameKind::TranscriptHypothesis, 0x8003),
        (SidecarFrameKind::PlaybackAccepted, 0x8004),
        (SidecarFrameKind::PlaybackRendered, 0x8005),
        (SidecarFrameKind::PlaybackFlushed, 0x8006),
        (SidecarFrameKind::CaptureStarted, 0x8007),
        (SidecarFrameKind::CapturePaused, 0x8008),
        (SidecarFrameKind::CaptureResumed, 0x8009),
        (SidecarFrameKind::Failure, 0x80fe),
        (SidecarFrameKind::ShutdownComplete, 0x80ff),
    ];

    for (kind, code) in expected {
        assert_eq!(kind.code(), code);
    }
}

#[test]
fn capture_controls_round_trip_exact_session_and_operation_identity() {
    let controls = [
        SidecarControl::StartCapture {
            session_id: SessionId::new(7),
            operation_id: 1,
        },
        SidecarControl::PauseCapture {
            session_id: SessionId::new(7),
            operation_id: 2,
        },
        SidecarControl::ResumeCapture {
            session_id: SessionId::new(7),
            operation_id: 3,
        },
        SidecarControl::CaptureStarted {
            session_id: SessionId::new(7),
            operation_id: 1,
        },
        SidecarControl::CapturePaused {
            session_id: SessionId::new(7),
            operation_id: 2,
        },
        SidecarControl::CaptureResumed {
            session_id: SessionId::new(7),
            operation_id: 3,
        },
    ];

    for control in controls {
        let frame = SidecarFrame::control(control);
        assert_eq!(decode_frame(&encode_frame(&frame).unwrap()).unwrap(), frame);
    }
}

#[test]
fn unknown_sidecar_protocol_version_is_rejected_explicitly() {
    let bytes = raw_frame(99, 0x0002, br#"{"session_id":7,"operation_id":1}"#);

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::UnknownVersion(99))
    );
}

#[test]
fn capture_controls_reject_zero_operation_identity() {
    let frame = SidecarFrame::control(SidecarControl::PauseCapture {
        session_id: SessionId::new(7),
        operation_id: 0,
    });
    assert_eq!(
        encode_frame(&frame),
        Err(SidecarCodecError::InvalidControlJson)
    );

    let bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::CapturePaused.code(),
        br#"{"session_id":7,"operation_id":0}"#,
    );
    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::InvalidControlJson)
    );
}

#[test]
fn capture_controls_reject_zero_session_identity() {
    let controls = [
        SidecarControl::StartCapture {
            session_id: SessionId::new(0),
            operation_id: 1,
        },
        SidecarControl::PauseCapture {
            session_id: SessionId::new(0),
            operation_id: 1,
        },
        SidecarControl::ResumeCapture {
            session_id: SessionId::new(0),
            operation_id: 1,
        },
        SidecarControl::CaptureStarted {
            session_id: SessionId::new(0),
            operation_id: 1,
        },
        SidecarControl::CapturePaused {
            session_id: SessionId::new(0),
            operation_id: 1,
        },
        SidecarControl::CaptureResumed {
            session_id: SessionId::new(0),
            operation_id: 1,
        },
    ];
    for control in controls {
        assert_eq!(
            encode_frame(&SidecarFrame::control(control)),
            Err(SidecarCodecError::InvalidControlJson)
        );
    }

    let kinds = [
        SidecarFrameKind::StartCapture,
        SidecarFrameKind::PauseCapture,
        SidecarFrameKind::ResumeCapture,
        SidecarFrameKind::CaptureStarted,
        SidecarFrameKind::CapturePaused,
        SidecarFrameKind::CaptureResumed,
    ];
    for kind in kinds {
        let bytes = raw_frame(
            PROTOCOL_VERSION,
            kind.code(),
            br#"{"session_id":0,"operation_id":1}"#,
        );
        assert_eq!(
            decode_frame(&bytes),
            Err(SidecarCodecError::InvalidControlJson)
        );
    }
}

#[test]
fn start_session_fixture_round_trips_exactly() {
    let bytes =
        include_bytes!("../../../../tests/fixtures/voice-sidecar-v1/control/start-session.bin");
    let frame = decode_frame(bytes).unwrap();

    assert_eq!(frame.version(), 1);
    assert_eq!(frame.kind(), SidecarFrameKind::StartSession);
    assert_eq!(encode_frame(&frame).unwrap(), bytes);
    assert_eq!(
        &bytes[HEADER_BYTES..],
        br#"{"session_id":7,"speech_start_ms":200,"final_silence_ms":600}"#
    );
}

#[test]
fn transcript_partial_fixture_round_trips_exactly() {
    let bytes = include_bytes!(
        "../../../../tests/fixtures/voice-sidecar-v1/control/transcript-partial.bin"
    );
    let frame = decode_frame(bytes).unwrap();

    assert_eq!(frame.kind(), SidecarFrameKind::TranscriptHypothesis);
    assert_eq!(encode_frame(&frame).unwrap(), bytes);
    assert_eq!(
        frame.as_control(),
        Some(&SidecarControl::TranscriptHypothesis {
            session_id: SessionId::new(7),
            hypothesis: RecognitionHypothesis::partial(3, "hel"),
        })
    );
    assert_eq!(
        &bytes[HEADER_BYTES..],
        br#"{"session_id":7,"segment_id":3,"text":"hel","engine_final":false}"#
    );
}

#[test]
fn signed_sixteen_audio_fixture_pins_metadata_and_pcm_bytes() {
    let frame = SidecarFrame::audio(
        SessionId::new(1),
        AudioFrame::new(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            5,
            PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            vec![0x00, 0x80, 0xff, 0x7f],
        )
        .unwrap(),
    );
    let bytes = encode_frame(&frame).unwrap();
    let frame = decode_frame(&bytes).unwrap();
    let (session_id, audio) = frame.as_audio().unwrap();

    assert_eq!(frame.kind(), SidecarFrameKind::AudioFrame);
    assert_eq!(session_id, SessionId::new(1));
    assert_eq!(audio.turn_id(), TurnId::new(2));
    assert_eq!(audio.generation_id(), GenerationId::new(3));
    assert_eq!(audio.utterance_id(), UtteranceId::new(4));
    assert_eq!(audio.sequence(), 5);
    assert_eq!(audio.format().sample_rate_hz(), 24_000);
    assert_eq!(audio.format().channels(), 1);
    assert_eq!(
        audio.format().sample_format(),
        PcmSampleFormat::Signed16LittleEndian
    );
    assert_eq!(audio.bytes(), &[0x00, 0x80, 0xff, 0x7f]);
    assert_eq!(encode_frame(&frame).unwrap(), bytes);

    let expected = [
        0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00, 0x34, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00, 0x05, 0x00, 0x00, 0x5d, 0xc0, 0x00, 0x01, 0x00, 0x01, 0x00, 0x80, 0xff, 0x7f,
    ];
    assert_eq!(bytes.as_slice(), expected);
}

#[test]
fn float_thirty_two_audio_uses_sample_format_code_two() {
    let frame = SidecarFrame::audio(
        SessionId::new(1),
        AudioFrame::new(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            5,
            PcmFormat::new(48_000, 2, PcmSampleFormat::Float32LittleEndian).unwrap(),
            vec![0; 8],
        )
        .unwrap(),
    );
    let encoded = encode_frame(&frame).unwrap();

    assert_eq!(&encoded[HEADER_BYTES + 46..HEADER_BYTES + 48], &[0, 2]);
    assert_eq!(decode_frame(&encoded).unwrap(), frame);
}

#[test]
fn every_partial_header_returns_need_more_data() {
    let complete = header(SidecarFrameKind::Ready, 0);

    for available in 0..HEADER_BYTES {
        assert_eq!(
            decode_frame(&complete[..available]),
            Err(SidecarCodecError::NeedMoreData {
                required: HEADER_BYTES,
                available,
            })
        );
    }
}

#[test]
fn partial_payload_returns_need_more_data_until_exact_length() {
    let complete = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::StartCapture.code(),
        br#"{"session_id":7}"#,
    );

    for available in HEADER_BYTES..complete.len() {
        assert_eq!(
            decode_frame(&complete[..available]),
            Err(SidecarCodecError::NeedMoreData {
                required: complete.len(),
                available,
            })
        );
    }
}

#[test]
fn eof_converts_partial_data_to_typed_truncation() {
    let complete = encode_frame(&SidecarFrame::control(SidecarControl::StartSession {
        session_id: SessionId::new(7),
        speech_start_ms: 200,
        final_silence_ms: 600,
    }))
    .unwrap();
    let bytes = &complete[..complete.len() - 3];
    let error = decode_frame_at_eof(bytes).unwrap_err();

    assert!(matches!(
        error,
        SidecarCodecError::TruncatedFrame {
            required: 69,
            available
        } if available == bytes.len()
    ));
}

#[test]
fn unknown_version_fails_before_payload_decode() {
    let bytes = raw_frame(99, SidecarFrameKind::StartCapture.code(), &[0xff]);

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::UnknownVersion(99))
    );
}

#[test]
fn unknown_kind_fails_before_payload_decode() {
    let bytes = raw_frame(PROTOCOL_VERSION, 0x7777, &[0xff]);

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::UnknownKind(0x7777))
    );
}

#[test]
fn invalid_control_utf8_is_typed() {
    let bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::StartCapture.code(),
        &[0xff],
    );

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::InvalidControlUtf8)
    );
}

#[test]
fn invalid_control_json_is_typed() {
    let bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::StartCapture.code(),
        br#"{"session_id":"wrong"}"#,
    );

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::InvalidControlJson)
    );
}

#[test]
fn control_json_denies_unknown_fields() {
    let bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::StartCapture.code(),
        br#"{"session_id":7,"unexpected":true}"#,
    );

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::InvalidControlJson)
    );
}

#[test]
fn declared_control_length_is_rejected_before_payload_read() {
    let bytes = header(
        SidecarFrameKind::StartSession,
        u32::try_from(MAX_CONTROL_PAYLOAD_BYTES + 1).unwrap(),
    );

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::PayloadTooLarge {
            kind: SidecarFrameKind::StartSession,
            declared: MAX_CONTROL_PAYLOAD_BYTES + 1,
            maximum: MAX_CONTROL_PAYLOAD_BYTES,
        })
    );
}

#[test]
fn maximum_control_payload_round_trips_and_one_more_byte_fails() {
    let exact = SidecarFrame::control(SidecarControl::TranscriptHypothesis {
        session_id: SessionId::new(1),
        hypothesis: RecognitionHypothesis::partial(1, "x".repeat(MAX_CONTROL_PAYLOAD_BYTES - 62)),
    });
    let encoded = encode_frame(&exact).unwrap();

    assert_eq!(encoded.len(), HEADER_BYTES + MAX_CONTROL_PAYLOAD_BYTES);
    assert_eq!(decode_frame(&encoded).unwrap(), exact);

    let oversized = SidecarFrame::control(SidecarControl::TranscriptHypothesis {
        session_id: SessionId::new(1),
        hypothesis: RecognitionHypothesis::partial(1, "x".repeat(MAX_CONTROL_PAYLOAD_BYTES - 61)),
    });
    assert_eq!(
        encode_frame(&oversized),
        Err(SidecarCodecError::PayloadTooLarge {
            kind: SidecarFrameKind::TranscriptHypothesis,
            declared: MAX_CONTROL_PAYLOAD_BYTES + 1,
            maximum: MAX_CONTROL_PAYLOAD_BYTES,
        })
    );
}

#[test]
fn maximum_complete_audio_payload_round_trips() {
    let pcm = vec![0; 65_536];
    let frame = SidecarFrame::audio(
        SessionId::new(1),
        AudioFrame::new(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            5,
            PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            pcm,
        )
        .unwrap(),
    );
    let encoded = encode_frame(&frame).unwrap();

    assert_eq!(AUDIO_METADATA_BYTES, 48);
    assert_eq!(encoded.len(), HEADER_BYTES + MAX_AUDIO_PAYLOAD_BYTES);
    assert_eq!(decode_frame(&encoded).unwrap(), frame);
}

#[test]
fn complete_audio_payload_above_maximum_fails_from_header_only() {
    let mut bytes = header(
        SidecarFrameKind::AudioFrame,
        u32::try_from(MAX_AUDIO_PAYLOAD_BYTES + 1).unwrap(),
    );
    bytes.extend_from_slice(&[0; 4]);

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::PayloadTooLarge {
            kind: SidecarFrameKind::AudioFrame,
            declared: MAX_AUDIO_PAYLOAD_BYTES + 1,
            maximum: MAX_AUDIO_PAYLOAD_BYTES,
        })
    );
}

#[test]
fn pcm_body_has_an_independent_maximum() {
    let payload = vec![0; AUDIO_METADATA_BYTES + 65_537];

    assert_eq!(
        decode_audio_payload(&payload),
        Err(SidecarCodecError::PcmPayloadTooLarge {
            declared: 65_537,
            maximum: 65_536,
        })
    );
}

#[test]
fn declared_u32_maximum_fails_without_allocating_payload() {
    let bytes = header(SidecarFrameKind::StartSession, u32::MAX);

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::PayloadTooLarge {
            kind: SidecarFrameKind::StartSession,
            declared: usize::try_from(u32::MAX).unwrap(),
            maximum: MAX_CONTROL_PAYLOAD_BYTES,
        })
    );
}

#[test]
fn audio_metadata_rejects_unknown_sample_format() {
    let mut payload = valid_audio_payload();
    payload[46..48].copy_from_slice(&3_u16.to_be_bytes());
    let bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::AudioFrame.code(),
        &payload,
    );

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::UnknownSampleFormat(3))
    );
}

#[test]
fn audio_metadata_rejects_zero_rate_and_channel_values() {
    let mut zero_rate = valid_audio_payload();
    zero_rate[40..44].copy_from_slice(&0_u32.to_be_bytes());
    let mut zero_channels = valid_audio_payload();
    zero_channels[44..46].copy_from_slice(&0_u16.to_be_bytes());

    assert_eq!(
        decode_audio_payload(&zero_rate),
        Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM sample rate must be greater than zero"
        ))
    );
    assert_eq!(
        decode_audio_payload(&zero_channels),
        Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM channels must be greater than zero"
        ))
    );
}

#[test]
fn audio_metadata_rejects_unaligned_pcm() {
    let mut payload = valid_audio_payload();
    payload.push(0);

    assert_eq!(
        decode_audio_payload(&payload),
        Err(SidecarCodecError::InvalidAudioMetadata(
            "PCM frame bytes were not sample aligned"
        ))
    );
}

#[test]
fn failure_payload_is_exactly_typed_and_content_free() {
    let frame = SidecarFrame::control(SidecarControl::Failure {
        session_id: SessionId::new(9),
        stage: RuntimeStage::SpeechRecognizer,
        code: SidecarFailureCode::PermissionDenied,
    });
    let encoded = encode_frame(&frame).unwrap();

    assert_eq!(
        &encoded[HEADER_BYTES..],
        br#"{"session_id":9,"stage":"speech_recognizer","code":"permission_denied"}"#
    );
    assert_eq!(decode_frame(&encoded).unwrap(), frame);
}

#[test]
fn failure_json_rejects_every_arbitrary_content_field() {
    let payloads = [
        br#"{"session_id":9,"stage":"speech_recognizer","code":"permission_denied","message":"private transcript"}"#.as_slice(),
        br#"{"session_id":9,"stage":"speech_recognizer","code":"permission_denied","text":"private transcript"}"#.as_slice(),
        br#"{"session_id":9,"stage":"speech_recognizer","code":"permission_denied","transcript":"private transcript"}"#.as_slice(),
        br#"{"session_id":9,"stage":"speech_recognizer","code":"permission_denied","content":"private transcript"}"#.as_slice(),
    ];

    for payload in payloads {
        let bytes = raw_frame(PROTOCOL_VERSION, SidecarFrameKind::Failure.code(), payload);
        assert_eq!(
            decode_frame(&bytes),
            Err(SidecarCodecError::InvalidControlJson)
        );
    }
}

#[test]
fn failure_json_rejects_unknown_string_code() {
    let bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::Failure.code(),
        br#"{"session_id":9,"stage":"speech_recognizer","code":"private transcript"}"#,
    );

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::InvalidControlJson)
    );
}

#[test]
fn control_events_round_trip_existing_voice_types() {
    let frames = [
        SidecarFrame::control(SidecarControl::VoiceActivity {
            session_id: SessionId::new(1),
            activity: VoiceActivity::SpeechStarted { at_ms: 10 },
        }),
        SidecarFrame::control(SidecarControl::VoiceActivity {
            session_id: SessionId::new(1),
            activity: VoiceActivity::SpeechContinued { at_ms: 20 },
        }),
        SidecarFrame::control(SidecarControl::VoiceActivity {
            session_id: SessionId::new(1),
            activity: VoiceActivity::SpeechEnded { at_ms: 30 },
        }),
        SidecarFrame::control(SidecarControl::TranscriptHypothesis {
            session_id: SessionId::new(1),
            hypothesis: RecognitionHypothesis::engine_final(2, "hello"),
        }),
    ];

    for frame in frames {
        let encoded = encode_frame(&frame).unwrap();
        assert_eq!(decode_frame(&encoded).unwrap(), frame);
    }
}

#[test]
fn every_control_kind_round_trips_with_exact_identity_fields() {
    let controls = [
        SidecarControl::StartSession {
            session_id: SessionId::new(1),
            speech_start_ms: 200,
            final_silence_ms: 600,
        },
        SidecarControl::StartCapture {
            session_id: SessionId::new(1),
            operation_id: 1,
        },
        SidecarControl::FlushGeneration {
            session_id: SessionId::new(1),
            generation_id: GenerationId::new(2),
            operation_id: 3,
        },
        SidecarControl::Shutdown {
            session_id: SessionId::new(1),
        },
        SidecarControl::Ready {
            session_id: SessionId::new(1),
        },
        SidecarControl::PlaybackAccepted {
            session_id: SessionId::new(1),
            turn_id: TurnId::new(2),
            generation_id: GenerationId::new(2),
            utterance_id: UtteranceId::new(4),
            sequence: 5,
        },
        SidecarControl::PlaybackRendered {
            session_id: SessionId::new(1),
            turn_id: TurnId::new(2),
            generation_id: GenerationId::new(2),
            utterance_id: UtteranceId::new(4),
            sequence: 5,
        },
        SidecarControl::PlaybackFlushed {
            session_id: SessionId::new(1),
            generation_id: GenerationId::new(2),
            operation_id: 3,
        },
        SidecarControl::Failure {
            session_id: SessionId::new(1),
            stage: RuntimeStage::AudioOutput,
            code: SidecarFailureCode::PlaybackFailed,
        },
        SidecarControl::ShutdownComplete {
            session_id: SessionId::new(1),
        },
    ];

    for control in controls {
        let frame = SidecarFrame::control(control);
        let encoded = encode_frame(&frame).unwrap();
        assert_eq!(decode_frame(&encoded).unwrap(), frame);
    }
}

#[test]
fn trailing_bytes_are_not_silently_accepted() {
    let mut bytes = raw_frame(
        PROTOCOL_VERSION,
        SidecarFrameKind::StartCapture.code(),
        br#"{"session_id":7}"#,
    );
    bytes.push(0);

    assert_eq!(
        decode_frame(&bytes),
        Err(SidecarCodecError::TrailingBytes(1))
    );
}

#[test]
#[ignore = "writes the immutable version-one cross-language fixtures once"]
fn write_version_one_fixtures() {
    let start_session = SidecarFrame::control(SidecarControl::StartSession {
        session_id: SessionId::new(7),
        speech_start_ms: 200,
        final_silence_ms: 600,
    });
    let transcript_partial = SidecarFrame::control(SidecarControl::TranscriptHypothesis {
        session_id: SessionId::new(7),
        hypothesis: RecognitionHypothesis::partial(3, "hel"),
    });
    let capture_controls = [
        (
            "start-capture.bin",
            SidecarControl::StartCapture {
                session_id: SessionId::new(7),
                operation_id: 1,
            },
        ),
        (
            "pause-capture.bin",
            SidecarControl::PauseCapture {
                session_id: SessionId::new(7),
                operation_id: 2,
            },
        ),
        (
            "resume-capture.bin",
            SidecarControl::ResumeCapture {
                session_id: SessionId::new(7),
                operation_id: 3,
            },
        ),
        (
            "capture-started.bin",
            SidecarControl::CaptureStarted {
                session_id: SessionId::new(7),
                operation_id: 1,
            },
        ),
        (
            "capture-paused.bin",
            SidecarControl::CapturePaused {
                session_id: SessionId::new(7),
                operation_id: 2,
            },
        ),
        (
            "capture-resumed.bin",
            SidecarControl::CaptureResumed {
                session_id: SessionId::new(7),
                operation_id: 3,
            },
        ),
    ];

    write_fixture(
        "control/start-session.bin",
        &encode_frame(&start_session).unwrap(),
    );
    write_fixture(
        "control/transcript-partial.bin",
        &encode_frame(&transcript_partial).unwrap(),
    );
    for (name, control) in capture_controls {
        write_fixture(
            &format!("control/{name}"),
            &encode_frame(&SidecarFrame::control(control)).unwrap(),
        );
    }
}

fn valid_audio_payload() -> Vec<u8> {
    let mut payload = Vec::with_capacity(AUDIO_METADATA_BYTES + 4);
    payload.extend_from_slice(&1_u64.to_be_bytes());
    payload.extend_from_slice(&2_u64.to_be_bytes());
    payload.extend_from_slice(&3_u64.to_be_bytes());
    payload.extend_from_slice(&4_u64.to_be_bytes());
    payload.extend_from_slice(&5_u64.to_be_bytes());
    payload.extend_from_slice(&24_000_u32.to_be_bytes());
    payload.extend_from_slice(&1_u16.to_be_bytes());
    payload.extend_from_slice(&1_u16.to_be_bytes());
    payload.extend_from_slice(&[0x00, 0x80, 0xff, 0x7f]);
    payload
}

fn raw_frame(version: u16, kind: u16, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES + payload.len());
    bytes.extend_from_slice(&version.to_be_bytes());
    bytes.extend_from_slice(&kind.to_be_bytes());
    bytes.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn header(kind: SidecarFrameKind, payload_length: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HEADER_BYTES);
    bytes.extend_from_slice(&PROTOCOL_VERSION.to_be_bytes());
    bytes.extend_from_slice(&kind.code().to_be_bytes());
    bytes.extend_from_slice(&payload_length.to_be_bytes());
    bytes
}

fn write_fixture(relative: &str, bytes: &[u8]) {
    let path = fixture_path(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, bytes).unwrap();
}

fn fixture_path(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(FIXTURE_ROOT)
        .join(relative)
}
