use conversation_model_adapters::{FrameProvenance, PcmFormat, PcmSampleFormat};

fn format() -> PcmFormat {
    PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap()
}

fn fixture() -> Vec<u8> {
    let value: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/timed-capture-v1/frame.json"
    ))
    .unwrap();
    let hex = value["timingHex"].as_str().unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

// Receipt delay is not an input to decoding: fixed source coordinates survive.
#[test]
fn canonical_frame_preserves_nominal_capture_and_exact_source_samples() {
    let bytes = fixture();
    let timing = FrameProvenance::decode(&bytes).unwrap();
    timing.validate_pcm(format(), 4).unwrap();
    assert_eq!(timing.capture_epoch, 1);
    assert_eq!(timing.sequence, 7);
    assert_eq!(timing.output_start_sample, 0);
    assert_eq!(timing.sample_count, 2);
    assert_eq!(timing.source_end_numerator, 96_000);
    let span = timing.span.as_ref().unwrap();
    assert_eq!(
        (span.host_start_ns, span.host_end_ns),
        (1_000_000_000, 1_000_083_334)
    );
    assert_eq!(span.capture_uncertainty_ns, None);
    assert_eq!(span.rounding_envelope_ns, 4);
    assert_eq!(timing.encode().unwrap().as_slice(), bytes);
}

#[test]
fn malformed_provenance_cannot_relabel_pcm_or_unknown_accuracy() {
    let canonical = fixture();
    for (offset, value) in [
        (0, 2),
        (1, 5),
        (2, 2),
        (3, 1),
        (11, 0),
        (31, 0),
        (55, 0),
        (87, 1),
    ] {
        let mut malformed = canonical.clone();
        malformed[offset] = value;
        assert!(
            FrameProvenance::decode(&malformed)
                .and_then(|p| p.validate_pcm(format(), 4))
                .is_err(),
            "offset {offset}"
        );
    }
    for length in [0, 87, 89] {
        let mut malformed = canonical.clone();
        malformed.resize(length, 0);
        assert!(FrameProvenance::decode(&malformed).is_err());
    }
    let timing = FrameProvenance::decode(&canonical).unwrap();
    assert!(timing.validate_pcm(format(), 2).is_err());
    assert!(timing
        .validate_pcm(
            PcmFormat::new(48_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            4
        )
        .is_err());
}

#[tokio::test]
async fn metadata_is_recorder_only_and_pcm_fanout_preserves_identical_provenance() {
    use conversation_model_adapters::{
        frame_for_network_capture, CaptureEpoch, CaptureEvent, CaptureMetadata, NetworkMediaHub,
        NetworkVoiceIoFactory, TimedCapture, VoiceInputEvent, VoiceIoFactory,
    };
    use conversation_protocol::SessionId;
    use tokio_util::sync::CancellationToken;
    let hub = NetworkMediaHub::default();
    let session = SessionId::new(41);
    hub.reserve(session, 1).await.unwrap();
    let factory = NetworkVoiceIoFactory::new(hub.clone(), session);
    let cancellation = CancellationToken::new();
    let io = factory.start(session, cancellation.clone()).await.unwrap();
    let mut voice = io.input.start(session, cancellation.clone()).await.unwrap();
    let capture = factory.attach_recognition_capture().await.unwrap();
    let mut recognition = capture.start(session, cancellation.clone()).await.unwrap();
    let epoch = CaptureEpoch {
        bridge_session_id: "bridge-independent".into(),
        clock_domain_id: "process-domain".into(),
        capture_epoch: 1,
    };
    hub.deliver_capture_metadata(session, CaptureMetadata::Epoch(epoch.clone()))
        .await
        .unwrap();
    assert_eq!(
        voice.recv().await.unwrap().unwrap(),
        VoiceInputEvent::CaptureMetadata(CaptureMetadata::Epoch(epoch.clone()))
    );
    assert!(recognition.try_recv().is_err());
    let frame = frame_for_network_capture(7, format(), vec![0, 0, 255, 127])
        .unwrap()
        .with_capture_timing(TimedCapture {
            epoch,
            frame: FrameProvenance::decode(&fixture()).unwrap(),
        })
        .unwrap();
    hub.deliver_capture(session, frame.clone()).await.unwrap();
    assert_eq!(
        voice.recv().await.unwrap().unwrap(),
        VoiceInputEvent::Capture(CaptureEvent::Frame(frame.clone()))
    );
    assert_eq!(
        recognition.recv().await.unwrap().unwrap(),
        CaptureEvent::Frame(frame)
    );
    assert!(voice.try_recv().is_err());
    assert!(recognition.try_recv().is_err());
    cancellation.cancel();
    io.completion.await.unwrap().unwrap();
    hub.shutdown().await;
}

#[test]
fn canonical_metadata_preserves_unsigned_maxima_signed_extremes_and_exact_lengths() {
    use conversation_model_adapters::{CaptureClockPair, CaptureEpoch};
    let fixture: serde_json::Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/timed-capture-v1/metadata.json"
    ))
    .unwrap();
    let bytes = |key: &str| {
        let hex = fixture[key].as_str().unwrap();
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect::<Vec<_>>()
    };
    let epoch = CaptureEpoch::decode(&bytes("epochBodyHex")).unwrap();
    assert_eq!(epoch.bridge_session_id, "bridge-independent");
    assert_eq!(epoch.clock_domain_id, "source-process");
    assert_eq!(epoch.encode().unwrap(), bytes("epochBodyHex"));
    let pair = CaptureClockPair::decode(&bytes("clockPairBodyHex")).unwrap();
    assert_eq!(pair.local_unix_ns, 2_000_000_000);
    assert_eq!(pair.external_utc_accuracy_ns, None);
    assert_eq!(pair.encode().unwrap(), bytes("clockPairBodyHex"));
    for (key, expected) in [
        ("signedPairMinimumBodyHex", i64::MIN),
        ("signedPairMaximumBodyHex", i64::MAX),
    ] {
        let pair = CaptureClockPair::decode(&bytes(key)).unwrap();
        assert_eq!(pair.local_unix_ns, expected);
        assert_eq!(pair.encode().unwrap(), bytes(key));
    }
    let max = FrameProvenance::decode(&bytes("maximumProvenanceHex")).unwrap();
    max.validate_pcm(
        PcmFormat::new(1, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
        2,
    )
    .unwrap();
    assert_eq!(max.sequence, u64::MAX);
    assert_eq!(max.source_end_numerator, u64::MAX);
    assert_eq!(max.span.unwrap().capture_uncertainty_ns, Some(u64::MAX));
    for key in ["epochBodyHex", "clockPairBodyHex"] {
        let mut oversized = bytes(key);
        oversized.push(0);
        assert!(if key == "epochBodyHex" {
            CaptureEpoch::decode(&oversized).is_err()
        } else {
            CaptureClockPair::decode(&oversized).is_err()
        });
    }
}
