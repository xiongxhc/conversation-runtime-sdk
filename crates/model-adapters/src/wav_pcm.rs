use std::mem::size_of;

use conversation_protocol::{GenerationId, TurnId, UtteranceId};

use crate::{
    AdapterError, AudioFormat, AudioFrame, PcmFormat, PcmSampleFormat, SynthesizedAudio,
    MAX_PCM_FRAME_BYTES,
};

const DEFAULT_MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const FRAME_DURATION_MILLISECONDS: u64 = 20;
const MILLISECONDS_PER_SECOND: u64 = 1_000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WavPcmDecoder {
    max_audio_bytes: usize,
}

impl Default for WavPcmDecoder {
    fn default() -> Self {
        Self {
            max_audio_bytes: DEFAULT_MAX_AUDIO_BYTES,
        }
    }
}

impl WavPcmDecoder {
    pub fn decode(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
        utterance_id: UtteranceId,
        audio: &SynthesizedAudio,
    ) -> Result<Vec<AudioFrame>, AdapterError> {
        self.decode_from_sequence(turn_id, generation_id, utterance_id, 0, audio)
    }

    pub(crate) fn decode_from_sequence(
        &self,
        turn_id: TurnId,
        generation_id: GenerationId,
        utterance_id: UtteranceId,
        initial_sequence: u64,
        audio: &SynthesizedAudio,
    ) -> Result<Vec<AudioFrame>, AdapterError> {
        if audio.format() == AudioFormat::Aiff {
            return Err(AdapterError::new(
                "R3 PCM streaming does not support AIFF audio",
            ));
        }

        let bytes = audio.bytes();
        if bytes.len() > self.max_audio_bytes {
            return Err(AdapterError::new("WAV audio exceeded the configured limit"));
        }

        let (format, data_bytes) = parse_wav(bytes, self.max_audio_bytes)?;
        let alignment = format.frame_alignment_bytes()?;
        if !data_bytes.is_multiple_of(alignment) {
            return Err(invalid_wav_error());
        }

        let frame_sample_numerator = frame_sample_numerator(format)?;
        validate_largest_frame(format, frame_sample_numerator)?;
        let sample_count = data_bytes / alignment;
        let frame_count_upper_bound =
            frame_count_upper_bound(sample_count, frame_sample_numerator)?;
        let frame_metadata_bytes = frame_count_upper_bound
            .checked_mul(size_of::<AudioFrame>())
            .ok_or_else(invalid_wav_error)?;
        if frame_metadata_bytes > self.max_audio_bytes {
            return Err(AdapterError::new("WAV audio produced too many PCM frames"));
        }

        let mut pcm_bytes = Vec::with_capacity(data_bytes);
        walk_chunks(bytes, |chunk_id, chunk_body| {
            if chunk_id == b"data" {
                pcm_bytes.extend_from_slice(chunk_body);
            }
            Ok(())
        })?;

        let mut frames = Vec::with_capacity(frame_count_upper_bound);
        let mut previous_sample_boundary = 0_usize;
        let mut frame_slot = 1_u64;
        let mut sequence = initial_sequence;
        while previous_sample_boundary < sample_count {
            let mut sample_boundary = sample_boundary(frame_slot, frame_sample_numerator)?;
            if sample_boundary <= previous_sample_boundary {
                frame_slot = next_frame_slot(previous_sample_boundary, frame_sample_numerator)?;
                continue;
            }
            if sample_boundary > sample_count {
                sample_boundary = sample_count;
            }

            let start = byte_offset(previous_sample_boundary, alignment)?;
            let end = byte_offset(sample_boundary, alignment)?;
            frames.push(AudioFrame::new(
                turn_id,
                generation_id,
                utterance_id,
                sequence,
                format,
                pcm_bytes[start..end].to_vec(),
            )?);
            previous_sample_boundary = sample_boundary;
            frame_slot = frame_slot.checked_add(1).ok_or_else(invalid_wav_error)?;
            sequence = sequence.checked_add(1).ok_or_else(invalid_wav_error)?;
        }

        Ok(frames)
    }
}

fn parse_wav(bytes: &[u8], max_audio_bytes: usize) -> Result<(PcmFormat, usize), AdapterError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid_wav_error());
    }

    let declared_size =
        usize::try_from(read_u32_le(&bytes[4..8])?).map_err(|_| invalid_wav_error())?;
    if declared_size
        .checked_add(8)
        .filter(|size| *size == bytes.len())
        .is_none()
    {
        return Err(invalid_wav_error());
    }

    let mut format = None;
    let mut data_bytes = 0_usize;
    walk_chunks(bytes, |chunk_id, chunk_body| match chunk_id {
        b"fmt " => {
            let next_format = parse_format(chunk_body)?;
            if let Some(previous_format) = format.replace(next_format) {
                if previous_format != next_format {
                    return Err(AdapterError::new("WAV PCM format changed between chunks"));
                }
            }
            Ok(())
        }
        b"data" => {
            data_bytes = data_bytes
                .checked_add(chunk_body.len())
                .filter(|size| *size <= max_audio_bytes)
                .ok_or_else(|| AdapterError::new("WAV audio exceeded the configured limit"))?;
            Ok(())
        }
        _ => Ok(()),
    })?;

    let format = format.ok_or_else(invalid_wav_error)?;
    if data_bytes == 0 {
        return Err(invalid_wav_error());
    }

    Ok((format, data_bytes))
}

fn parse_format(bytes: &[u8]) -> Result<PcmFormat, AdapterError> {
    let header = bytes.get(..16).ok_or_else(invalid_wav_error)?;
    let audio_format = read_u16_le(&header[..2])?;
    let channels = read_u16_le(&header[2..4])?;
    let sample_rate_hz = read_u32_le(&header[4..8])?;
    let byte_rate = read_u32_le(&header[8..12])?;
    let block_align = read_u16_le(&header[12..14])?;
    let bits_per_sample = read_u16_le(&header[14..16])?;

    if audio_format != 1 || bits_per_sample != 16 || !(1..=2).contains(&channels) {
        return Err(AdapterError::new(
            "WAV PCM format is unsupported for R3 streaming",
        ));
    }

    let format = PcmFormat::new(
        sample_rate_hz,
        channels,
        PcmSampleFormat::Signed16LittleEndian,
    )?;
    let expected_block_align =
        u16::try_from(format.frame_alignment_bytes()?).map_err(|_| invalid_wav_error())?;
    let expected_byte_rate = sample_rate_hz
        .checked_mul(u32::from(expected_block_align))
        .ok_or_else(invalid_wav_error)?;
    if block_align != expected_block_align || byte_rate != expected_byte_rate {
        return Err(invalid_wav_error());
    }

    Ok(format)
}

fn frame_sample_numerator(format: PcmFormat) -> Result<u64, AdapterError> {
    u64::from(format.sample_rate_hz())
        .checked_mul(FRAME_DURATION_MILLISECONDS)
        .ok_or_else(invalid_wav_error)
}

fn validate_largest_frame(
    format: PcmFormat,
    frame_sample_numerator: u64,
) -> Result<(), AdapterError> {
    let largest_sample_count = frame_sample_numerator
        .checked_add(MILLISECONDS_PER_SECOND - 1)
        .ok_or_else(invalid_wav_error)?
        / MILLISECONDS_PER_SECOND;
    largest_sample_count
        .checked_mul(
            u64::try_from(format.frame_alignment_bytes()?).map_err(|_| invalid_wav_error())?,
        )
        .and_then(|size| usize::try_from(size).ok())
        .filter(|size| *size > 0 && *size <= MAX_PCM_FRAME_BYTES)
        .ok_or_else(|| AdapterError::new("WAV PCM frame size is unsupported for R3 streaming"))?;
    Ok(())
}

fn frame_count_upper_bound(
    sample_count: usize,
    frame_sample_numerator: u64,
) -> Result<usize, AdapterError> {
    let sample_count = u64::try_from(sample_count).map_err(|_| invalid_wav_error())?;
    let frame_count = sample_count
        .checked_mul(MILLISECONDS_PER_SECOND)
        .and_then(|value| value.checked_add(frame_sample_numerator - 1))
        .map(|value| value / frame_sample_numerator)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(invalid_wav_error)?;
    Ok(frame_count.min(usize::try_from(sample_count).map_err(|_| invalid_wav_error())?))
}

fn sample_boundary(frame_slot: u64, frame_sample_numerator: u64) -> Result<usize, AdapterError> {
    frame_slot
        .checked_mul(frame_sample_numerator)
        .map(|value| value / MILLISECONDS_PER_SECOND)
        .and_then(|boundary| usize::try_from(boundary).ok())
        .ok_or_else(invalid_wav_error)
}

fn next_frame_slot(
    previous_sample_boundary: usize,
    frame_sample_numerator: u64,
) -> Result<u64, AdapterError> {
    u64::try_from(previous_sample_boundary)
        .map_err(|_| invalid_wav_error())?
        .checked_add(1)
        .and_then(|value| value.checked_mul(MILLISECONDS_PER_SECOND))
        .and_then(|value| value.checked_add(frame_sample_numerator - 1))
        .map(|value| value / frame_sample_numerator)
        .filter(|slot| *slot > 0)
        .ok_or_else(invalid_wav_error)
}

fn byte_offset(sample_count: usize, alignment: usize) -> Result<usize, AdapterError> {
    sample_count
        .checked_mul(alignment)
        .ok_or_else(invalid_wav_error)
}

fn walk_chunks(
    bytes: &[u8],
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    let mut offset = 12_usize;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or_else(invalid_wav_error)?;
        let header = bytes
            .get(offset..header_end)
            .ok_or_else(invalid_wav_error)?;
        let body_end = header_end
            .checked_add(
                usize::try_from(read_u32_le(&header[4..8])?).map_err(|_| invalid_wav_error())?,
            )
            .ok_or_else(invalid_wav_error)?;
        let next_offset = body_end
            .checked_add((body_end - header_end) % 2)
            .ok_or_else(invalid_wav_error)?;
        let body = bytes
            .get(header_end..body_end)
            .ok_or_else(invalid_wav_error)?;
        bytes
            .get(body_end..next_offset)
            .ok_or_else(invalid_wav_error)?;
        visit(&header[..4], body)?;
        offset = next_offset;
    }
    Ok(())
}

fn read_u16_le(bytes: &[u8]) -> Result<u16, AdapterError> {
    let bytes: [u8; 2] = bytes.try_into().map_err(|_| invalid_wav_error())?;
    Ok(u16::from_le_bytes(bytes))
}

fn read_u32_le(bytes: &[u8]) -> Result<u32, AdapterError> {
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| invalid_wav_error())?;
    Ok(u32::from_le_bytes(bytes))
}

fn invalid_wav_error() -> AdapterError {
    AdapterError::new("WAV audio was malformed for R3 PCM streaming")
}
