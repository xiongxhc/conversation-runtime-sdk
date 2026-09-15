//! The online batch transcription and diarization adapter, against a scripted provider.
//!
//! Every test drives a loopback `TcpListener` whose replies the test writes itself. No real
//! provider is reached, and the credential is a fixed non-secret string.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use conversation_model_adapters::{
    AdapterError, DiarizationResult, DiarizationWarning, MediaSampleOrigin, MeetingAudioWindow,
    OpenAiDiarizationConfig, OpenAiTranscriptionDiarizer, PcmFormat, PcmSampleFormat,
    TranscriptionDiarizer,
};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex, Notify};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_util::sync::CancellationToken;

/// A fixed non-secret value. No test reaches a real provider.
const CREDENTIAL_VALUE: &str = "fake-not-a-real-credential-0000";
const CREDENTIAL_ENV: &str = "CONVERSATION_TEST_ONLINE_DIARIZATION_CREDENTIAL";
const PRIVATE_TEXT: &str = "private meeting transcript";
/// The identity a provider might volunteer. It must never reach the result.
const PROVIDER_IDENTITY: &str = "Chris Xiong";
/// Three seconds of 16 kHz mono signed 16-bit audio, so a provider timing beyond three
/// seconds is outside the window it was cut from.
const WINDOW_BYTES: usize = 96_000;

#[tokio::test]
async fn exact_origin_is_added_before_legacy_floor_and_never_narrowed_to_float() {
    set_credential();
    for (origin, expected_start, expected_end) in [
        (524_288, 21_969, 21_969),
        (
            9_007_199_254_740_993,
            375_299_968_947_665,
            375_299_968_947_665,
        ),
    ] {
        let server = DiarizationServer::start(vec![Reply::body(json!({
            "task":"transcribe", "duration": 1.0, "text":"fractional",
            "segments":[{"id":"s", "type":"transcript.text.segment", "start":16.0/24_000.0,
                "end":24.5/24_000.0, "text":"fractional", "speaker":"A"}]
        }))])
        .await;
        let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
        let window = MeetingAudioWindow::new_with_sample_origin(
            format,
            MediaSampleOrigin::new(origin, 24_000, 123).unwrap(),
            vec![0; 80],
        )
        .unwrap();
        let result = diarize(&server, [window], CancellationToken::new())
            .await
            .unwrap();
        let range = result.segments()[0]
            .media_sample_range()
            .expect("exact03 sample range");
        assert_eq!(range.start_sample_inclusive(), origin + 16);
        assert_eq!(range.end_sample_exclusive(), origin + 25);
        assert_eq!(range.sample_rate_hz(), 24_000);
        assert_eq!(range.gap_offset_ms(), 123);
        assert_eq!(result.segments()[0].started_at_ms(), expected_start);
        assert_eq!(result.segments()[0].ended_at_ms(), expected_end);
        assert_eq!(result.segments()[0].text(), "fractional");
    }
}

#[tokio::test]
async fn documented_multipart_upload_preserves_wav_samples_and_fractional_response() {
    set_credential();
    for language in [None, Some("en")] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/v1/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_raw_request(&mut stream).await.unwrap();
            write_json(
                &mut stream,
                200,
                &json!({
                    "task": "transcribe", "duration": 0.002, "text": "brief wording",
                    "segments": [{"id":"provider-0", "type":"transcript.text.segment",
                        "start":0.0, "end":0.0015, "text":"brief wording", "speaker":"A"}],
                    "usage":{"type":"duration", "seconds":1}, "future_field":true
                })
                .to_string(),
            )
            .await;
            request
        });
        let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
        let pcm = [0x21, 0x43].repeat(40);
        let window = MeetingAudioWindow::new(format, 700, pcm.clone()).unwrap();
        let mut config = OpenAiDiarizationConfig::new(&endpoint, "diarize-1", CREDENTIAL_ENV)
            .unwrap()
            .with_request_timeout(Duration::from_secs(5))
            .unwrap();
        if let Some(language) = language {
            config = config.with_language(language).unwrap();
        }
        let result = timeout(
            Duration::from_secs(5),
            OpenAiTranscriptionDiarizer::new(config)
                .diarize(window_channel([window]), CancellationToken::new()),
        )
        .await
        .unwrap();
        let request = timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            request.headers.lines().next(),
            Some("POST /v1/audio/transcriptions HTTP/1.1")
        );
        assert_eq!(
            header_value(&request.headers, "authorization"),
            Some(format!("Bearer {CREDENTIAL_VALUE}"))
        );
        let content_type = header_value(&request.headers, "content-type").unwrap();
        let boundary = content_type
            .strip_prefix("multipart/form-data; boundary=")
            .unwrap();
        let parts = multipart_parts(&request.body, boundary);
        assert_eq!(parts.len(), if language.is_some() { 5 } else { 4 });
        let field = |name: &str| {
            parts
                .iter()
                .find(|(headers, _)| headers.contains(&format!("name=\"{name}\"")))
                .unwrap()
        };
        assert_eq!(field("model").1, b"diarize-1");
        assert_eq!(field("response_format").1, b"diarized_json");
        assert_eq!(field("chunking_strategy").1, b"auto");
        if let Some(language) = language {
            assert_eq!(field("language").1, language.as_bytes());
            assert_eq!(
                parts
                    .iter()
                    .filter(|(headers, _)| headers.contains("name=\"language\""))
                    .count(),
                1
            );
        }
        let (headers, wav) = field("file");
        assert!(headers.contains("filename=\"window.wav\""));
        assert_eq!(
            header_value(headers, "content-type").as_deref(),
            Some("audio/wav")
        );
        assert_eq!(wav.len(), 124);
        assert_eq!(&wav[..12], b"RIFF\x74\0\0\0WAVE");
        assert_eq!(
            &wav[12..36],
            b"fmt \x10\0\0\0\x01\0\x01\0\xc0\x5d\0\0\x80\xbb\0\0\x02\0\x10\0"
        );
        assert_eq!(&wav[36..44], b"data\x50\0\0\0");
        assert_eq!(&wav[44..], pcm);
        assert!(!request
            .body
            .windows(CREDENTIAL_VALUE.len())
            .any(|bytes| bytes == CREDENTIAL_VALUE.as_bytes()));
        let result = result.expect("documented schema is supported without duration equality");
        assert_eq!(result.segments()[0].text(), "brief wording");
        assert_eq!(result.segments()[0].started_at_ms(), 700);
        assert_eq!(result.segments()[0].ended_at_ms(), 701);
        assert_eq!(result.segments()[0].speaker_cluster(), Some("cluster-1"));
    }
}

struct RawRequest {
    headers: String,
    body: Vec<u8>,
}

fn multipart_parts<'a>(body: &'a [u8], boundary: &str) -> Vec<(String, &'a [u8])> {
    let marker = format!("--{boundary}");
    let mut remaining = body;
    let mut parts = Vec::new();
    loop {
        remaining = remaining.strip_prefix(marker.as_bytes()).unwrap();
        if remaining == b"--\r\n" {
            break;
        }
        remaining = remaining.strip_prefix(b"\r\n").unwrap();
        let header_end = remaining
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = String::from_utf8(remaining[..header_end].to_vec()).unwrap();
        remaining = &remaining[header_end + 4..];
        let delimiter = format!("\r\n{marker}");
        let end = remaining
            .windows(delimiter.len())
            .position(|bytes| bytes == delimiter.as_bytes())
            .unwrap();
        parts.push((headers, &remaining[..end]));
        remaining = &remaining[end + 2..];
    }
    parts
}

#[tokio::test]
async fn invalid_window_bounds_fail_before_credential_or_http() {
    let server = DiarizationServer::start([]).await;
    let diarizer = OpenAiTranscriptionDiarizer::new(
        OpenAiDiarizationConfig::new(
            server.endpoint(),
            "configured-model",
            "CONVERSATION_TASK12_ABSENT_CREDENTIAL",
        )
        .unwrap(),
    );
    let cases = [
        MeetingAudioWindow::new(
            PcmFormat::new(24_000, 1, PcmSampleFormat::Float32LittleEndian).unwrap(),
            0,
            vec![0; 4],
        )
        .unwrap(),
        MeetingAudioWindow::new(
            PcmFormat::new(u32::MAX, 1, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            0,
            vec![0; 2],
        )
        .unwrap(),
        MeetingAudioWindow::new(
            PcmFormat::new(24_000, u16::MAX, PcmSampleFormat::Signed16LittleEndian).unwrap(),
            0,
            vec![0; usize::from(u16::MAX) * 2],
        )
        .unwrap(),
        MeetingAudioWindow::new(format(), 0, vec![0; 1024 * 1024 + 2]).unwrap(),
        window(u64::MAX - 1),
    ];
    for window in cases {
        let error = diarizer
            .diarize(window_channel([window]), CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            !error.message().contains("credential"),
            "window validation must precede credential resolution: {error}"
        );
    }
    assert_eq!(server.request_count().await, 0);
    server.finish().await;
}

#[tokio::test]
async fn raw_seconds_are_bounded_before_flooring_and_submillisecond_wording_survives() {
    set_credential();
    let extent = 40.0_f64 / 24_000.0;
    let just_above = f64::from_bits(extent.to_bits() + 1);
    let just_below = f64::from_bits(extent.to_bits() - 1);
    for timed in [false, true] {
        for (start, end, expected) in [
            (0.0, extent, Some((100, 101, 0, 40))),
            (0.0, just_below, Some((100, 101, 0, 40))),
            (0.0, just_above, None),
            (0.0001, 0.0002, Some((100, 100, 2, 5))),
            (-0.0001, 0.0002, None),
            (0.0002, 0.0001, None),
            (0.0001, 0.0001, None),
            (0.0, 0.5, None),
        ] {
            let server = DiarizationServer::start([Reply::body(json!({
            "task":"transcribe", "duration":99.0, "text":"short",
            "segments":[{"id":"p0", "type":"transcript.text.segment", "start":start, "end":end, "text":"short", "speaker":"A"}]
        }))]).await;
            let format = PcmFormat::new(24_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap();
            let window = if timed {
                MeetingAudioWindow::new_with_sample_origin(
                    format,
                    MediaSampleOrigin::new(0, 24_000, 100).unwrap(),
                    vec![0; 80],
                )
            } else {
                MeetingAudioWindow::new(format, 100, vec![0; 80])
            }
            .unwrap();
            let result = diarize(&server, [window], CancellationToken::new()).await;
            match expected {
                Some((start, end, sample_start, sample_end)) => {
                    let result = result.unwrap();
                    let segment = &result.segments()[0];
                    assert_eq!(
                        (segment.started_at_ms(), segment.ended_at_ms()),
                        (start, end)
                    );
                    assert_eq!(segment.text(), "short");
                    if timed {
                        let range = segment.media_sample_range().unwrap();
                        assert_eq!(
                            (range.start_sample_inclusive(), range.end_sample_exclusive()),
                            (sample_start, sample_end)
                        );
                    } else {
                        assert_eq!(segment.media_sample_range(), None);
                    }
                }
                None => assert!(
                    result.is_err(),
                    "raw interval {start}..{end} must fail before rounding"
                ),
            }
        }
    }
}

#[tokio::test]
async fn one_mib_pcm_is_accepted_with_exactly_one_wav_header() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(empty_answer())]).await;
    let window = MeetingAudioWindow::new(format(), 0, vec![0x71; 1024 * 1024]).unwrap();
    let result = diarize(&server, [window], CancellationToken::new())
        .await
        .unwrap();
    assert!(result.segments().is_empty());
    let requests = server.requests.lock().await;
    assert_eq!(requests.len(), 1);
    let wav = &requests[0].payload;
    assert_eq!(wav.len(), 1024 * 1024 + 44);
    assert_eq!(
        u32::from_le_bytes(wav[4..8].try_into().unwrap()),
        1024 * 1024 + 36
    );
    assert_eq!(
        u32::from_le_bytes(wav[40..44].try_into().unwrap()),
        1024 * 1024
    );
    assert!(wav[44..].iter().all(|byte| *byte == 0x71));
}

#[tokio::test]
async fn documented_required_fields_and_local_result_bounds_fail_closed_without_content() {
    set_credential();
    let segment = json!({"id":"p0","type":"transcript.text.segment","start":0.0,"end":0.5,"text":PRIVATE_TEXT,"speaker":"A"});
    let base = json!({"task":"transcribe","duration":3.0,"text":PRIVATE_TEXT,"segments":[segment]});
    let mut invalid = Vec::new();
    for key in ["task", "duration", "text", "segments"] {
        let mut value = base.clone();
        value.as_object_mut().unwrap().remove(key);
        invalid.push(value);
    }
    for key in ["id", "type", "start", "end", "text", "speaker"] {
        let mut value = base.clone();
        value["segments"][0].as_object_mut().unwrap().remove(key);
        invalid.push(value);
    }
    for (key, value) in [
        ("task", json!("translate")),
        ("duration", json!(-1)),
        ("duration", json!("NaN")),
        ("segments", json!([])),
    ] {
        let mut answer = base.clone();
        answer[key] = value;
        invalid.push(answer);
    }
    for (key, value) in [
        ("id", json!("")),
        ("id", json!("x".repeat(257))),
        ("speaker", json!("")),
        ("speaker", json!("😀".repeat(65))),
        ("speaker", Value::Null),
        ("type", json!("other")),
        ("start", json!("NaN")),
        ("end", json!("Infinity")),
        ("end", json!(1.0e308)),
        ("text", json!("x".repeat(2049))),
        ("text", json!("private\nwording")),
    ] {
        let mut answer = base.clone();
        answer["segments"][0][key] = value;
        invalid.push(answer);
    }
    let mut duplicate = base.clone();
    duplicate["segments"] = json!([segment.clone(), segment.clone()]);
    invalid.push(duplicate);
    for (count, text_bytes, distinct_speakers) in [(65, 1, true), (65, 2048, false)] {
        let mut answer = base.clone();
        answer["segments"] = Value::Array(
            (0..count)
                .map(|index| {
                    let mut segment = segment.clone();
                    segment["id"] = json!(format!("p{index}"));
                    segment["text"] = json!("x".repeat(text_bytes));
                    if distinct_speakers {
                        segment["speaker"] = json!(format!("provider-{index}"));
                    }
                    segment
                })
                .collect(),
        );
        invalid.push(answer);
    }
    for answer in invalid {
        let server = DiarizationServer::start([Reply::body(answer)]).await;
        let error = diarize(&server, [window(0)], CancellationToken::new())
            .await
            .unwrap_err();
        assert!(
            !error.message().contains(PRIVATE_TEXT)
                && !error.message().contains(CREDENTIAL_VALUE)
                && !error.message().contains("provider-")
        );
        assert_eq!(server.request_count().await, 1);
    }
    let mut maximum = base;
    maximum["segments"][0]["id"] = json!("😀".repeat(64));
    maximum["segments"][0]["speaker"] = json!("😀".repeat(64));
    let server = DiarizationServer::start([Reply::body(maximum)]).await;
    assert_eq!(
        diarize(&server, [window(0)], CancellationToken::new())
            .await
            .unwrap()
            .segments()[0]
            .speaker_cluster(),
        Some("cluster-1")
    );
}

#[tokio::test]
async fn clustered_segments_carry_anonymous_labels_on_the_media_timeline() {
    set_credential();
    let server = DiarizationServer::start([
        Reply::body(json!({
            "task":"transcribe", "duration":3.0, "text":"good morning morning",
            "segments": [
                { "id":"p0", "type":"transcript.text.segment", "start":0.0, "end":1.2, "text": "good morning", "speaker": "A" },
                { "id":"p1", "type":"transcript.text.segment", "start":1.2, "end":2.0, "text": "morning", "speaker": "B" },
            ],
            "warnings": [],
        })),
        Reply::body(json!({
            "task":"transcribe", "duration":3.0, "text":"shall we start",
            "segments": [
                { "id":"p0", "type":"transcript.text.segment", "start":0.0, "end":0.9, "text": "shall we start", "speaker": "A" },
            ],
            "warnings": [],
        })),
    ])
    .await;

    let result = diarize(
        &server,
        [window(0), window(30_000)],
        CancellationToken::new(),
    )
    .await
    .expect("a scripted provider answers");

    let segments = result.segments();
    assert_eq!(segments.len(), 3);
    assert_eq!(segments[0].text(), "good morning");
    assert_eq!(segments[0].started_at_ms(), 0);
    assert_eq!(segments[0].ended_at_ms(), 1_200);
    assert_eq!(segments[0].speaker_cluster(), Some("cluster-1"));
    assert_eq!(segments[1].speaker_cluster(), Some("cluster-2"));
    // The second window began 30 seconds along the media timeline, so its provider-relative
    // timings must be rebased onto that timeline rather than restarting at zero.
    assert_eq!(segments[2].started_at_ms(), 30_000);
    assert_eq!(segments[2].ended_at_ms(), 30_900);
    assert_eq!(
        segments[2].speaker_cluster(),
        Some("cluster-3"),
        "a label returned for another independent provider request is not meeting-wide evidence"
    );
    let identifiers = segments
        .iter()
        .map(|segment| segment.segment_id().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(identifiers.len(), 3);
    assert!(
        identifiers[0] != identifiers[1] && identifiers[1] != identifiers[2],
        "segment identifiers are unique across the whole meeting"
    );
    assert!(result.warnings().is_empty());
    assert_eq!(server.request_count().await, 2);
    assert_eq!(
        server.authorization().await.as_deref(),
        Some(format!("Bearer {CREDENTIAL_VALUE}").as_str()),
        "the credential travels in the Authorization header only"
    );
    assert_eq!(
        server.target(0).await,
        "/audio/transcriptions",
        "the batch endpoint is its own, not the streaming recognizer's"
    );
    assert_eq!(
        server.request(0).await,
        server.request(1).await,
        "equal PCM windows upload identically without disclosing media origins"
    );
}

#[tokio::test]
async fn a_provider_identity_never_reaches_the_result() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "task":"transcribe", "duration":3.0, "text":"hello hi",
        "segments": [
            { "id":"p0", "type":"transcript.text.segment", "start":0.0, "end":0.8, "text": "hello", "speaker": PROVIDER_IDENTITY },
            { "id":"p1", "type":"transcript.text.segment", "start":0.8, "end":0.9, "text": "hi", "speaker": "speaker-two" },
        ],
        "warnings": [],
    }))])
    .await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("a scripted provider answers");

    assert_eq!(result.segments()[0].speaker_cluster(), Some("cluster-1"));
    assert_eq!(result.segments()[1].speaker_cluster(), Some("cluster-2"));
    let rendered = format!("{result:?}");
    assert!(
        !rendered.contains(PROVIDER_IDENTITY),
        "a provider's own label must not survive anywhere in the result: {rendered}"
    );
    assert!(!rendered.contains("speaker-two"));
}

#[tokio::test]
async fn missing_required_provider_speaker_is_not_guessed() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "task":"transcribe", "duration":3.0, "text":"one voice only",
        "segments": [
            { "id":"p0", "type":"transcript.text.segment", "start":0.0, "end":0.8, "text": "one voice only", "speaker": null },
        ],
        "warnings": [],
    }))])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("documented speaker is required");
    assert_eq!(
        error.message(),
        "online diarization returned an unsupported answer"
    );
}

#[tokio::test]
async fn a_provider_that_hears_nothing_says_so_rather_than_failing() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(empty_answer())]).await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("an empty answer is an answer");

    assert!(result.segments().is_empty());
    assert_eq!(result.warnings(), [DiarizationWarning::NoSpeechDetected]);
}

#[tokio::test]
async fn overlapping_speech_is_kept_rather_than_flattened() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "task":"transcribe", "duration":3.0, "text":"so the plan is sorry, one moment",
        "segments": [
            { "id":"p0", "type":"transcript.text.segment", "start":0.0, "end":2.0, "text": "so the plan is", "speaker": "A" },
            { "id":"p1", "type":"transcript.text.segment", "start":1.5, "end":2.4, "text": "sorry, one moment", "speaker": "B" },
        ],
        "warnings": ["overlap"],
    }))])
    .await;

    let result = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect("a scripted provider answers");

    let segments = result.segments();
    assert_eq!(segments.len(), 2, "an overlap loses neither speaker");
    assert!(
        segments[1].started_at_ms() < segments[0].ended_at_ms(),
        "the two segments genuinely overlap on the timeline"
    );
    assert_ne!(segments[0].speaker_cluster(), segments[1].speaker_cluster());
    assert!(
        result.warnings().is_empty(),
        "unknown provider metadata is not a new runtime warning contract"
    );
    assert!(!format!("{result:?}").contains("overlap"));
}

#[tokio::test]
async fn malformed_provider_timings_fail_closed_without_echoing_the_transcript() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "task":"transcribe", "duration":3.0, "text":PRIVATE_TEXT,
        "segments": [
            { "id":"p0", "type":"transcript.text.segment", "start":4.0, "end":1.0, "text": PRIVATE_TEXT, "speaker": "A" },
        ],
        "warnings": [],
    }))])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a segment that ends before it starts is not a result");

    assert_eq!(
        error.message(),
        "online diarization returned a segment with unusable timings"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains(CREDENTIAL_VALUE));
}

#[tokio::test]
async fn a_segment_that_runs_past_its_window_fails_closed() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(json!({
        "task":"transcribe", "duration":3.0, "text":"far too long",
        "segments": [
            { "id":"p0", "type":"transcript.text.segment", "start":0.0, "end":9999.999, "text": "far too long", "speaker":"A" },
        ],
        "warnings": [],
    }))])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a segment cannot outlast the audio it was cut from");

    assert_eq!(
        error.message(),
        "online diarization returned a segment with unusable timings"
    );
}

#[tokio::test]
async fn a_cancelled_diarization_stops_without_a_result() {
    set_credential();
    let server = DiarizationServer::start([Reply::Gated(empty_answer().to_string())]).await;
    let cancellation = CancellationToken::new();
    let diarizer = diarizer(server.endpoint());
    let windows = window_channel([window(0)]);

    let task = {
        let cancellation = cancellation.clone();
        tokio::spawn(async move { diarizer.diarize(windows, cancellation).await })
    };
    server.wait_for_request_count(1).await;
    cancellation.cancel();

    let error = timeout(Duration::from_secs(5), task)
        .await
        .expect("a cancelled diarization must not hang")
        .expect("the diarization task must not panic")
        .expect_err("a cancelled diarization has no result");
    assert_eq!(error.message(), "online diarization was cancelled");
    server.release();
    server.finish().await;
}

#[tokio::test]
async fn one_absolute_deadline_covers_response_headers_and_streamed_body() {
    set_credential();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_raw_request(&mut stream).await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let body = empty_answer().to_string();
        stream.write_all(format!("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\n1\r\n{}\r\n", &body[..1]).as_bytes()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(600)).await;
        let rest = &body[1..];
        let _ = stream
            .write_all(format!("{:x}\r\n{rest}\r\n0\r\n\r\n", rest.len()).as_bytes())
            .await;
    });
    let adapter = OpenAiTranscriptionDiarizer::new(
        OpenAiDiarizationConfig::new(endpoint, "fixture", CREDENTIAL_ENV)
            .unwrap()
            .with_request_timeout(Duration::from_secs(1))
            .unwrap(),
    );
    let result = timeout(
        Duration::from_secs(5),
        adapter.diarize(window_channel([window(0)]), CancellationToken::new()),
    )
    .await
    .unwrap();
    timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.unwrap_err().message(),
        "online diarization response stalled"
    );
}

#[tokio::test]
async fn cancellation_during_incomplete_response_body_closes_owned_request_and_window_receiver() {
    set_credential();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (body_started, body_ready) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_raw_request(&mut stream).await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nContent-Type: application/json\r\n\r\n{").await.unwrap();
        body_started.send(()).unwrap();
        let mut byte = [0];
        matches!(
            timeout(Duration::from_secs(5), stream.read(&mut byte))
                .await
                .unwrap(),
            Ok(0) | Err(_)
        )
    });
    let adapter = diarizer(&endpoint);
    let cancel = CancellationToken::new();
    let (sender, receiver) = mpsc::channel(1);
    sender.send(Ok(window(0))).await.unwrap();
    let token = cancel.clone();
    let task = tokio::spawn(async move { adapter.diarize(receiver, token).await });
    timeout(Duration::from_secs(5), body_ready)
        .await
        .unwrap()
        .unwrap();
    sender.send(Ok(window(3000))).await.unwrap();
    cancel.cancel();
    let result = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    let closed = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(closed && sender.is_closed());
    assert_eq!(
        result.unwrap_err().message(),
        "online diarization was cancelled"
    );
}

#[tokio::test]
async fn cancellation_during_backpressured_upload_stops_before_the_full_wav_reaches_peer() {
    set_credential();
    let listener = tokio::net::TcpSocket::new_v4().unwrap();
    listener.set_recv_buffer_size(1024).unwrap();
    listener.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let listener = listener.listen(1).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (headers_sent, headers_ready) = tokio::sync::oneshot::channel();
    let (release, released) = tokio::sync::oneshot::channel();
    let peer = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut headers = Vec::new();
        while !headers.ends_with(b"\r\n\r\n") {
            assert!(headers.len() < 16_384);
            headers.push(stream.read_u8().await.unwrap());
        }
        let headers = String::from_utf8(headers).unwrap();
        let declared = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>().unwrap())
            })
            .unwrap();
        headers_sent.send(()).unwrap();
        released.await.unwrap();
        let mut received = 0;
        let mut chunk = [0; 8192];
        loop {
            match timeout(Duration::from_secs(5), stream.read(&mut chunk))
                .await
                .unwrap()
            {
                Ok(0) | Err(_) => break,
                Ok(count) => {
                    received += count;
                    assert!(received <= declared);
                }
            }
        }
        (received, declared)
    });
    let adapter = diarizer(&endpoint);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let pcm = MeetingAudioWindow::new(format(), 0, vec![0x35; 1024 * 1024]).unwrap();
    let task = tokio::spawn(async move { adapter.diarize(window_channel([pcm]), token).await });
    timeout(Duration::from_secs(5), headers_ready)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    let result = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    let (received, declared) = timeout(Duration::from_secs(5), peer)
        .await
        .unwrap()
        .unwrap();
    assert!(
        received < declared,
        "upload must be cancelled while still incomplete: {received}/{declared}"
    );
    assert_eq!(
        result.unwrap_err().message(),
        "online diarization was cancelled"
    );
}

#[tokio::test]
async fn a_provider_outage_fails_the_whole_reconciliation() {
    set_credential();
    let body = format!("{PRIVATE_TEXT} {CREDENTIAL_VALUE} {PROVIDER_IDENTITY}");
    let server = DiarizationServer::start([Reply::Failure { status: 503, body }]).await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("an outage is not a partial transcript");

    assert_eq!(
        error.message(),
        "online diarization request failed with HTTP status 503"
    );
    assert!(!error.message().contains(PRIVATE_TEXT));
    assert!(!error.message().contains(CREDENTIAL_VALUE));
    assert!(!error.message().contains(PROVIDER_IDENTITY));
}

#[tokio::test]
async fn a_dropped_provider_connection_fails_the_whole_reconciliation() {
    set_credential();
    let server = DiarizationServer::start([Reply::Reset]).await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a dropped connection is not a partial transcript");

    assert_eq!(error.message(), "online diarization transport failed");
}

#[tokio::test]
async fn a_response_without_a_declared_length_is_capped_as_it_arrives() {
    set_credential();
    // Sixteen mebibytes against a half-mebibyte cap, chunk-encoded so no Content-Length is
    // declared and the pre-check has nothing to look at.
    const OFFERED_BYTES: usize = 16 * 1024 * 1024;
    let server = DiarizationServer::start([Reply::Endless {
        total_bytes: OFFERED_BYTES,
    }])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("a response past the cap is not an answer");

    assert_eq!(
        error.message(),
        "online diarization response exceeded the configured limit",
        "the cap must fire, not the JSON parser after the whole body landed"
    );
    assert!(
        server.written_bytes() < OFFERED_BYTES,
        "the adapter must hang up mid-body rather than buffer all {OFFERED_BYTES} bytes; \
         the provider wrote {}",
        server.written_bytes()
    );
}

#[tokio::test]
async fn an_answer_larger_than_the_runtime_can_state_is_refused() {
    set_credential();
    let segments = (0..1_025)
        .map(
            |index| json!({"id":format!("p{index}"), "type":"transcript.text.segment", "start":0.0, "end":1.0, "text":"x", "speaker":"A"}),
        )
        .collect::<Vec<_>>();
    let server = DiarizationServer::start([Reply::body(
        json!({ "task":"transcribe", "duration":3.0, "text":"x", "segments": segments }),
    )])
    .await;

    let error = diarize(&server, [window(0)], CancellationToken::new())
        .await
        .expect_err("an oversized answer is refused rather than truncated");

    assert_eq!(error.message(), "diarization exceeded its segment limit");
}

#[tokio::test]
async fn a_window_the_recording_could_not_supply_fails_the_reconciliation() {
    set_credential();
    let server = DiarizationServer::start([Reply::body(empty_answer())]).await;
    let diarizer = diarizer(server.endpoint());
    let (sender, receiver) = mpsc::channel(1);
    sender
        .send(Err(AdapterError::new("recording could not be read")))
        .await
        .unwrap();
    drop(sender);

    let error = diarizer
        .diarize(receiver, CancellationToken::new())
        .await
        .expect_err("a recording that cannot be read has no transcript");

    assert_eq!(error.message(), "recording could not be read");
    assert_eq!(server.request_count().await, 0);
    server.finish().await;
}

// --- helpers ---------------------------------------------------------------------------------

fn empty_answer() -> Value {
    json!({"task":"transcribe", "duration":3.0, "text":"", "segments":[]})
}

fn set_credential() {
    std::env::set_var(CREDENTIAL_ENV, CREDENTIAL_VALUE);
}

fn format() -> PcmFormat {
    PcmFormat::new(16_000, 1, PcmSampleFormat::Signed16LittleEndian).unwrap()
}

fn window(started_at_ms: u64) -> MeetingAudioWindow {
    MeetingAudioWindow::new(format(), started_at_ms, vec![0x11; WINDOW_BYTES]).unwrap()
}

fn window_channel<I>(windows: I) -> mpsc::Receiver<Result<MeetingAudioWindow, AdapterError>>
where
    I: IntoIterator<Item = MeetingAudioWindow>,
{
    let windows = windows.into_iter().collect::<Vec<_>>();
    let (sender, receiver) = mpsc::channel(windows.len().max(1));
    for window in windows {
        sender.try_send(Ok(window)).unwrap();
    }
    receiver
}

fn diarizer(endpoint: &str) -> OpenAiTranscriptionDiarizer {
    OpenAiTranscriptionDiarizer::new(
        OpenAiDiarizationConfig::new(endpoint, "diarize-1", CREDENTIAL_ENV)
            .unwrap()
            .with_request_timeout(Duration::from_secs(5))
            .unwrap(),
    )
}

async fn diarize<I>(
    server: &DiarizationServer,
    windows: I,
    cancellation: CancellationToken,
) -> Result<DiarizationResult, AdapterError>
where
    I: IntoIterator<Item = MeetingAudioWindow>,
{
    let diarizer = diarizer(server.endpoint());
    let result = timeout(
        Duration::from_secs(5),
        diarizer.diarize(window_channel(windows), cancellation),
    )
    .await
    .expect("the diarization stalled");
    server.finish().await;
    result
}

enum Reply {
    Body(String),
    Gated(String),
    Failure {
        status: u16,
        body: String,
    },
    /// A chunked reply that declares no length and keeps sending. The adapter must refuse it
    /// on the way in rather than after it has all arrived.
    Endless {
        total_bytes: usize,
    },
    Reset,
}

impl Reply {
    fn body(value: Value) -> Self {
        Self::Body(value.to_string())
    }
}

/// One request the scripted provider received: its body, its Authorization header, and the
/// path it was sent to.
struct RecordedRequest {
    payload: Vec<u8>,
    authorization: Option<String>,
    target: String,
}

struct DiarizationServer {
    endpoint: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    gate: Arc<Notify>,
    /// How many body bytes the server managed to write before the adapter hung up.
    written: Arc<AtomicUsize>,
    cancellation: CancellationToken,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl DiarizationServer {
    async fn start<I>(replies: I) -> Self
    where
        I: IntoIterator<Item = Reply>,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new(Notify::new());
        let written = Arc::new(AtomicUsize::new(0));
        let worker_requests = requests.clone();
        let worker_gate = gate.clone();
        let worker_written = written.clone();
        let replies = Arc::new(Mutex::new(replies.into_iter().collect::<VecDeque<_>>()));
        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();

        let worker = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (mut stream, _) = tokio::select! {
                    biased;
                    _ = worker_cancellation.cancelled() => break,
                    result = listener.accept() => result.unwrap(),
                };
                let requests = worker_requests.clone();
                let replies = replies.clone();
                let gate = worker_gate.clone();
                let written = worker_written.clone();
                let cancellation = worker_cancellation.clone();
                connections.spawn(async move {
                    tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {},
                    _ = async {
                    while let Some(request) = read_request(&mut stream).await {
                        requests.lock().await.push(request);
                        let reply = replies.lock().await.pop_front().unwrap_or_else(|| {
                            Reply::body(empty_answer())
                        });
                        if !write_reply(&mut stream, reply, &gate, &written).await {
                            break;
                        }
                    }
                    } => {},
                    }
                });
            }
            while let Some(result) = connections.join_next().await {
                result.unwrap();
            }
        });

        Self {
            endpoint,
            requests,
            gate,
            written,
            cancellation,
            worker: Mutex::new(Some(worker)),
        }
    }

    fn endpoint(&self) -> &str {
        &self.endpoint
    }

    fn release(&self) {
        self.gate.notify_waiters();
    }

    async fn request_count(&self) -> usize {
        self.requests.lock().await.len()
    }

    async fn request(&self, index: usize) -> Vec<u8> {
        self.requests.lock().await[index].payload.clone()
    }

    async fn authorization(&self) -> Option<String> {
        self.requests.lock().await[0].authorization.clone()
    }

    async fn target(&self, index: usize) -> String {
        self.requests.lock().await[index].target.clone()
    }

    fn written_bytes(&self) -> usize {
        self.written.load(Ordering::SeqCst)
    }

    async fn finish(&self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.lock().await.take() {
            timeout(Duration::from_secs(5), worker)
                .await
                .unwrap()
                .unwrap();
        }
    }

    async fn wait_for_request_count(&self, expected: usize) {
        timeout(Duration::from_secs(5), async {
            while self.request_count().await < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the provider never received {expected} requests"));
    }
}

impl Drop for DiarizationServer {
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// Returns false when the connection must be closed after this reply.
async fn write_reply(
    stream: &mut TcpStream,
    reply: Reply,
    gate: &Notify,
    written: &AtomicUsize,
) -> bool {
    match reply {
        Reply::Body(body) => {
            write_json(stream, 200, &body).await;
            true
        }
        Reply::Gated(body) => {
            gate.notified().await;
            write_json(stream, 200, &body).await;
            true
        }
        Reply::Failure { status, body } => {
            write_json(stream, status, &body).await;
            true
        }
        Reply::Endless { total_bytes } => {
            // No Content-Length at all, which is the case the declared-length pre-check
            // cannot see.
            if stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
                )
                .await
                .is_err()
            {
                return false;
            }
            let payload = "x".repeat(64 * 1024);
            let mut sent = 0;
            while sent < total_bytes {
                let frame = format!("{:x}\r\n{payload}\r\n", payload.len());
                if stream.write_all(frame.as_bytes()).await.is_err() {
                    break;
                }
                sent += payload.len();
                written.store(sent, Ordering::SeqCst);
                tokio::task::yield_now().await;
            }
            false
        }
        Reply::Reset => false,
    }
}

async fn write_json(stream: &mut TcpStream, status: u16, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(body.as_bytes()).await.unwrap();
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let request = read_raw_request(stream).await?;
    Some(RecordedRequest {
        payload: multipart_parts(
            &request.body,
            header_value(&request.headers, "content-type")
                .unwrap()
                .strip_prefix("multipart/form-data; boundary=")
                .unwrap(),
        )
        .into_iter()
        .find(|(headers, _)| headers.contains("name=\"file\""))
        .unwrap()
        .1
        .to_vec(),
        authorization: header_value(&request.headers, "authorization"),
        target: request
            .headers
            .lines()
            .next()?
            .split_whitespace()
            .nth(1)?
            .to_owned(),
    })
}

async fn read_raw_request(stream: &mut TcpStream) -> Option<RawRequest> {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
        assert!(
            request.len() <= 2 * 1024 * 1024,
            "scripted request exceeded its bound"
        );
        if let Some(header_end) = request
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
        {
            break header_end;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
    let content_length = header_value(&headers, "content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap();
    assert!(content_length <= 2 * 1024 * 1024);
    while request.len() - header_end < content_length {
        let read = stream.read(&mut buffer).await.ok()?;
        if read == 0 {
            return None;
        }
        request.extend_from_slice(&buffer[..read]);
    }

    Some(RawRequest {
        headers,
        body: request[header_end..header_end + content_length].to_vec(),
    })
}

fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        line.split_once(':').and_then(|(header, value)| {
            header
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    })
}
