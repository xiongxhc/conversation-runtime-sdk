use std::mem::size_of;

use conversation_protocol::{GenerationId, TurnId, UtteranceId};

use crate::{
    AdapterError, AudioFormat, AudioFrame, PcmFormat, PcmSampleFormat, SynthesizedAudio,
    MAX_PCM_FRAME_BYTES,
};

const DEFAULT_MAX_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const FRAME_DURATION_MILLISECONDS: u64 = 20;

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
        let frame_bytes = frame_bytes(format)?;
        let frame_count = data_bytes
            .checked_add(frame_bytes - 1)
            .ok_or_else(invalid_wav_error)?
            / frame_bytes;
        let frame_metadata_bytes = frame_count
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

        let alignment = format.frame_alignment_bytes()?;
        if !pcm_bytes.len().is_multiple_of(alignment) {
            return Err(invalid_wav_error());
        }

        let mut frames = Vec::with_capacity(frame_count);
        for (sequence, bytes) in pcm_bytes.chunks(frame_bytes).enumerate() {
            frames.push(AudioFrame::new(
                turn_id,
                generation_id,
                utterance_id,
                u64::try_from(sequence).map_err(|_| invalid_wav_error())?,
                format,
                bytes.to_vec(),
            )?);
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

fn frame_bytes(format: PcmFormat) -> Result<usize, AdapterError> {
    let samples_per_frame = u64::from(format.sample_rate_hz())
        .checked_mul(FRAME_DURATION_MILLISECONDS)
        .ok_or_else(invalid_wav_error)?
        / 1_000;
    let frame_bytes = samples_per_frame
        .checked_mul(
            u64::try_from(format.frame_alignment_bytes()?).map_err(|_| invalid_wav_error())?,
        )
        .and_then(|size| usize::try_from(size).ok())
        .filter(|size| *size > 0 && *size <= MAX_PCM_FRAME_BYTES)
        .ok_or_else(|| AdapterError::new("WAV PCM frame size is unsupported for R3 streaming"))?;
    Ok(frame_bytes)
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
