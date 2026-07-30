use conversation_model_adapters::{
    AudioFormat, PcmSampleFormat, SynthesizedAudio, WavPcmDecoder, MAX_PCM_FRAME_BYTES,
};
use conversation_protocol::{GenerationId, TurnId, UtteranceId};

const MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;

#[test]
fn pcm16_wav_becomes_ordered_twenty_millisecond_frames() {
    let audio = pcm16_wav(24_000, 1, vec![0_i16; 1_200]);

    let frames = WavPcmDecoder::default()
        .decode(
            TurnId::new(2),
            GenerationId::new(3),
            UtteranceId::new(4),
            &audio,
        )
        .unwrap();

    assert_eq!(frames.len(), 3);
    assert_eq!(frames[0].bytes().len(), 960);
    assert_eq!(frames[1].sequence(), 1);
    assert_eq!(frames[2].bytes().len(), 480);
    assert_eq!(frames[2].turn_id(), TurnId::new(2));
    assert_eq!(frames[2].generation_id(), GenerationId::new(3));
    assert_eq!(frames[2].utterance_id(), UtteranceId::new(4));
    assert_eq!(
        frames[0].format().sample_format(),
        PcmSampleFormat::Signed16LittleEndian
    );
}

#[test]
fn pcm16_wav_uses_cumulative_boundaries_for_non_divisible_sample_rates() {
    let samples = (0..11_025).map(|sample| sample as i16).collect();
    let audio = pcm16_wav(11_025, 1, samples);

    let frames = decode(&audio).unwrap();

    assert_eq!(frames.len(), 50);
    for (sequence, frame) in frames.iter().enumerate() {
        assert_eq!(frame.sequence(), sequence as u64);
        assert_eq!(
            frame.bytes().len() / 2,
            if sequence % 2 == 0 { 220 } else { 221 }
        );
    }
}

#[test]
fn decoder_preserves_order_across_multiple_data_chunks() {
    let audio = wav_with_chunks([
        (*b"fmt ", pcm_format_body(1, 24_000, 1, 16)),
        (*b"data", vec![1, 0, 2, 0]),
        (*b"data", vec![3, 0, 4, 0]),
    ]);

    let frames = decode(&audio).unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].bytes(), &[1, 0, 2, 0, 3, 0, 4, 0]);
}

#[test]
fn decoder_accepts_identical_format_chunks_and_preserves_stereo_interleaving() {
    let stereo_bytes = vec![2, 1, 254, 255, 4, 3, 252, 255];
    let audio = wav_with_chunks([
        (*b"fmt ", pcm_format_body(1, 24_000, 2, 16)),
        (*b"data", stereo_bytes.clone()),
        (*b"fmt ", pcm_format_body(1, 24_000, 2, 16)),
    ]);

    let frames = decode(&audio).unwrap();

    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].format().channels(), 2);
    assert_eq!(frames[0].bytes(), stereo_bytes);
}

#[test]
fn decoder_rejects_checked_riff_and_chunk_boundaries() {
    let mut declared_size_mismatch = pcm16_wav(24_000, 1, vec![0_i16; 1]).bytes().to_vec();
    declared_size_mismatch[4..8].copy_from_slice(&1_u32.to_le_bytes());

    let mut missing_odd_padding = wav_with_chunks([
        (*b"fmt ", pcm_format_body(1, 24_000, 1, 16)),
        (*b"JUNK", vec![1]),
        (*b"data", vec![0, 0]),
    ])
    .bytes()
    .to_vec();
    missing_odd_padding.remove(45);
    set_riff_size(&mut missing_odd_padding);

    let mut oversized_declared_chunk = Vec::from(&b"RIFF\0\0\0\0WAVE"[..]);
    append_chunk(
        &mut oversized_declared_chunk,
        b"fmt ",
        &pcm_format_body(1, 24_000, 1, 16),
    );
    oversized_declared_chunk.extend_from_slice(b"data");
    oversized_declared_chunk.extend_from_slice(&u32::MAX.to_le_bytes());
    set_riff_size(&mut oversized_declared_chunk);

    for bytes in [
        declared_size_mismatch,
        missing_odd_padding,
        oversized_declared_chunk,
    ] {
        assert!(decode(&SynthesizedAudio::new(bytes, AudioFormat::Wav)).is_err());
    }
}

#[test]
fn decoder_rejects_malformed_or_unsupported_wav_containers() {
    let valid = pcm16_wav(24_000, 1, vec![0_i16; 1]);
    let mut trailing_chunk = valid.bytes().to_vec();
    trailing_chunk.push(0);
    set_riff_size(&mut trailing_chunk);

    let cases = [
        SynthesizedAudio::new(b"not audio".to_vec(), AudioFormat::Wav),
        wav_with_chunks([(*b"data", vec![0, 0])]),
        wav_with_chunks([(*b"fmt ", pcm_format_body(1, 24_000, 1, 16))]),
        wav_with_chunks([
            (*b"fmt ", pcm_format_body(1, 24_000, 1, 16)),
            (*b"data", Vec::new()),
        ]),
        wav_with_chunks([
            (*b"fmt ", pcm_format_body(3, 24_000, 1, 16)),
            (*b"data", vec![0, 0]),
        ]),
        wav_with_chunks([
            (*b"fmt ", pcm_format_body(1, 24_000, 1, 24)),
            (*b"data", vec![0, 0]),
        ]),
        wav_with_chunks([
            (*b"fmt ", pcm_format_body(1, 24_000, 3, 16)),
            (*b"data", vec![0, 0]),
        ]),
        wav_with_chunks([
            (*b"fmt ", pcm_format_body(1, 0, 1, 16)),
            (*b"data", vec![0, 0]),
        ]),
        SynthesizedAudio::new(trailing_chunk, AudioFormat::Wav),
    ];

    for audio in cases {
        assert!(WavPcmDecoder::default()
            .decode(
                TurnId::new(2),
                GenerationId::new(3),
                UtteranceId::new(4),
                &audio,
            )
            .is_err());
    }
}

#[test]
fn decoder_rejects_aiff_with_explicit_unsupported_container_error() {
    let audio = SynthesizedAudio::new(minimal_aiff(), AudioFormat::Aiff);

    let error = decode(&audio).unwrap_err();

    assert_eq!(
        error.message(),
        "R3 PCM streaming does not support AIFF audio"
    );
}

#[test]
fn decoder_rejects_oversized_and_changing_format_wav_chunks() {
    let oversized = pcm16_wav(24_000, 1, vec![0_i16; MAX_AUDIO_BYTES / 2]);
    let changing_format = wav_with_chunks([
        (*b"fmt ", pcm_format_body(1, 24_000, 1, 16)),
        (*b"data", vec![0, 0]),
        (*b"fmt ", pcm_format_body(1, 48_000, 1, 16)),
    ]);
    let oversized_frame = pcm16_wav(2_000_000, 2, vec![0_i16; 80_000]);

    for audio in [oversized, changing_format, oversized_frame] {
        assert!(WavPcmDecoder::default()
            .decode(
                TurnId::new(2),
                GenerationId::new(3),
                UtteranceId::new(4),
                &audio,
            )
            .is_err());
    }

    assert_eq!(MAX_PCM_FRAME_BYTES, 64 * 1024);
}

fn decode(
    audio: &SynthesizedAudio,
) -> Result<Vec<conversation_model_adapters::AudioFrame>, conversation_model_adapters::AdapterError>
{
    WavPcmDecoder::default().decode(
        TurnId::new(2),
        GenerationId::new(3),
        UtteranceId::new(4),
        audio,
    )
}

fn pcm16_wav(sample_rate_hz: u32, channels: u16, samples: Vec<i16>) -> SynthesizedAudio {
    let mut data = Vec::with_capacity(samples.len() * 2);
    for sample in samples {
        data.extend_from_slice(&sample.to_le_bytes());
    }
    wav_with_chunks([
        (*b"fmt ", pcm_format_body(1, sample_rate_hz, channels, 16)),
        (*b"data", data),
    ])
}

fn wav_with_chunks<const N: usize>(chunks: [([u8; 4], Vec<u8>); N]) -> SynthesizedAudio {
    let mut bytes = Vec::from(&b"RIFF\0\0\0\0WAVE"[..]);
    for (chunk_id, chunk_body) in chunks {
        append_chunk(&mut bytes, &chunk_id, &chunk_body);
    }
    set_riff_size(&mut bytes);
    SynthesizedAudio::new(bytes, AudioFormat::Wav)
}

fn append_chunk(bytes: &mut Vec<u8>, chunk_id: &[u8; 4], chunk_body: &[u8]) {
    bytes.extend_from_slice(chunk_id);
    bytes.extend_from_slice(&u32::try_from(chunk_body.len()).unwrap().to_le_bytes());
    bytes.extend_from_slice(chunk_body);
    if chunk_body.len() % 2 == 1 {
        bytes.push(0);
    }
}

fn pcm_format_body(audio_format: u16, sample_rate_hz: u32, channels: u16, bits: u16) -> Vec<u8> {
    let block_align = channels * (bits / 8);
    let byte_rate = sample_rate_hz * u32::from(block_align);
    [
        audio_format.to_le_bytes().as_slice(),
        channels.to_le_bytes().as_slice(),
        sample_rate_hz.to_le_bytes().as_slice(),
        byte_rate.to_le_bytes().as_slice(),
        block_align.to_le_bytes().as_slice(),
        bits.to_le_bytes().as_slice(),
    ]
    .concat()
}

fn set_riff_size(bytes: &mut [u8]) {
    let size = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&size.to_le_bytes());
}

fn minimal_aiff() -> Vec<u8> {
    let mut bytes = Vec::from(&b"FORM\0\0\0\0AIFF"[..]);
    bytes.extend_from_slice(b"COMM");
    bytes.extend_from_slice(&18_u32.to_be_bytes());
    bytes.extend_from_slice(&[0; 18]);
    bytes.extend_from_slice(b"SSND");
    bytes.extend_from_slice(&9_u32.to_be_bytes());
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.extend_from_slice(&0_u32.to_be_bytes());
    bytes.push(0);
    bytes.push(0);
    let form_size = u32::try_from(bytes.len() - 8).unwrap();
    bytes[4..8].copy_from_slice(&form_size.to_be_bytes());
    bytes
}
