use conversation_protocol::TurnId;
use tokio_util::sync::CancellationToken;

use crate::{AdapterError, AdapterFuture};

const INVALID_ENCODED_CONTAINER: &str = "synthesized audio was not a valid encoded container";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum AudioFormat {
    Aiff,
    Wav,
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SynthesizedAudio {
    bytes: Vec<u8>,
    format: AudioFormat,
    sample_rate_hz: Option<u32>,
    channels: Option<u16>,
}

impl SynthesizedAudio {
    pub fn new<I>(bytes: I, format: AudioFormat) -> Self
    where
        I: IntoIterator<Item = u8>,
    {
        Self {
            bytes: bytes.into_iter().collect(),
            format,
            sample_rate_hz: None,
            channels: None,
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub const fn format(&self) -> AudioFormat {
        self.format
    }

    pub const fn sample_rate_hz(&self) -> Option<u32> {
        self.sample_rate_hz
    }

    pub const fn channels(&self) -> Option<u16> {
        self.channels
    }

    pub const fn with_sample_rate_hz(mut self, sample_rate_hz: u32) -> Self {
        self.sample_rate_hz = Some(sample_rate_hz);
        self
    }

    pub const fn with_channels(mut self, channels: u16) -> Self {
        self.channels = Some(channels);
        self
    }

    pub fn validate(&self) -> Result<(), AdapterError> {
        match self.format {
            AudioFormat::Aiff => validate_aiff(&self.bytes),
            AudioFormat::Wav => validate_wav(&self.bytes),
        }
    }
}

fn validate_wav(bytes: &[u8]) -> Result<(), AdapterError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(invalid_container_error());
    }
    if read_u32_le(&bytes[4..8])?
        .checked_add(8)
        .filter(|size| *size == bytes.len())
        .is_none()
    {
        return Err(invalid_container_error());
    }

    let mut has_format = false;
    let mut has_data = false;
    walk_chunks(bytes, 12, read_u32_le, |chunk_id, chunk_body| {
        match chunk_id {
            b"fmt " if chunk_body.len() >= 16 => has_format = true,
            b"fmt " => return Err(invalid_container_error()),
            b"data" if !chunk_body.is_empty() => has_data = true,
            _ => {}
        }
        Ok(())
    })?;

    if has_format && has_data {
        Ok(())
    } else {
        Err(invalid_container_error())
    }
}

fn validate_aiff(bytes: &[u8]) -> Result<(), AdapterError> {
    if bytes.len() < 12 || &bytes[..4] != b"FORM" {
        return Err(invalid_container_error());
    }
    let form_type = &bytes[8..12];
    let minimum_comm_size = match form_type {
        b"AIFF" => 18,
        b"AIFC" => 22,
        _ => return Err(invalid_container_error()),
    };
    if read_u32_be(&bytes[4..8])?
        .checked_add(8)
        .filter(|size| *size == bytes.len())
        .is_none()
    {
        return Err(invalid_container_error());
    }

    let mut has_comm = false;
    let mut has_ssnd = false;
    walk_chunks(bytes, 12, read_u32_be, |chunk_id, chunk_body| {
        match chunk_id {
            b"COMM" if chunk_body.len() >= minimum_comm_size => has_comm = true,
            b"COMM" => return Err(invalid_container_error()),
            b"SSND" => {
                let offset = read_u32_be(chunk_body.get(..4).ok_or_else(invalid_container_error)?)?;
                let sound_data_start = 8_usize
                    .checked_add(offset)
                    .filter(|start| *start < chunk_body.len())
                    .ok_or_else(invalid_container_error)?;
                if chunk_body.get(4..8).is_none() || chunk_body.get(sound_data_start..).is_none() {
                    return Err(invalid_container_error());
                }
                has_ssnd = true;
            }
            _ => {}
        }
        Ok(())
    })?;

    if has_comm && has_ssnd {
        Ok(())
    } else {
        Err(invalid_container_error())
    }
}

fn walk_chunks(
    bytes: &[u8],
    mut offset: usize,
    read_size: fn(&[u8]) -> Result<usize, AdapterError>,
    mut visit: impl FnMut(&[u8], &[u8]) -> Result<(), AdapterError>,
) -> Result<(), AdapterError> {
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or_else(invalid_container_error)?;
        let header = bytes
            .get(offset..header_end)
            .ok_or_else(invalid_container_error)?;
        let chunk_size = read_size(&header[4..8])?;
        let body_end = header_end
            .checked_add(chunk_size)
            .ok_or_else(invalid_container_error)?;
        let next_offset = body_end
            .checked_add(chunk_size % 2)
            .ok_or_else(invalid_container_error)?;
        let chunk_body = bytes
            .get(header_end..body_end)
            .ok_or_else(invalid_container_error)?;
        bytes
            .get(body_end..next_offset)
            .ok_or_else(invalid_container_error)?;
        visit(&header[..4], chunk_body)?;
        offset = next_offset;
    }
    Ok(())
}

fn read_u32_le(bytes: &[u8]) -> Result<usize, AdapterError> {
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| invalid_container_error())?;
    Ok(u32::from_le_bytes(bytes) as usize)
}

fn read_u32_be(bytes: &[u8]) -> Result<usize, AdapterError> {
    let bytes: [u8; 4] = bytes.try_into().map_err(|_| invalid_container_error())?;
    Ok(u32::from_be_bytes(bytes) as usize)
}

fn invalid_container_error() -> AdapterError {
    AdapterError::new(INVALID_ENCODED_CONTAINER)
}

#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct SpeechRequest {
    turn_id: TurnId,
    text: String,
}

impl SpeechRequest {
    pub fn new(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            turn_id,
            text: text.into(),
        }
    }

    pub const fn turn_id(&self) -> TurnId {
        self.turn_id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

pub trait SpeechSynthesizer: Send + Sync {
    /// Implementations must observe `cancellation` and resolve only after
    /// cleaning up work owned by the request.
    fn synthesize<'a>(
        &'a self,
        request: SpeechRequest,
        cancellation: CancellationToken,
    ) -> AdapterFuture<'a, SynthesizedAudio>;
}

#[cfg(test)]
mod tests {
    use super::{AudioFormat, SynthesizedAudio};

    const INVALID_CONTAINER: &str = "synthesized audio was not a valid encoded container";

    #[test]
    fn synthesized_audio_exposes_declared_format_and_optional_metadata() {
        let audio = SynthesizedAudio::new([1, 2, 3], AudioFormat::Aiff)
            .with_sample_rate_hz(22_050)
            .with_channels(1);

        assert_eq!(audio.bytes(), &[1, 2, 3]);
        assert_eq!(audio.format(), AudioFormat::Aiff);
        assert_eq!(audio.sample_rate_hz(), Some(22_050));
        assert_eq!(audio.channels(), Some(1));
    }

    #[test]
    fn typed_audio_accepts_minimal_wav_and_aiff_containers() {
        for audio in [
            SynthesizedAudio::new(minimal_wav(), AudioFormat::Wav),
            SynthesizedAudio::new(minimal_aiff(b"AIFF", 18), AudioFormat::Aiff),
            SynthesizedAudio::new(minimal_aiff(b"AIFC", 22), AudioFormat::Aiff),
        ] {
            assert!(audio.validate().is_ok());
        }
    }

    #[test]
    fn typed_audio_rejects_malformed_containers() {
        let valid_wav = minimal_wav();
        let valid_aiff = minimal_aiff(b"AIFF", 18);

        let mut wav_wrong_size = valid_wav.clone();
        wav_wrong_size[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut aiff_wrong_size = valid_aiff.clone();
        aiff_wrong_size[4..8].copy_from_slice(&u32::MAX.to_be_bytes());

        let mut wav_truncated_padding = valid_wav[..valid_wav.len() - 1].to_vec();
        set_riff_size(&mut wav_truncated_padding);
        let mut aiff_truncated_padding = valid_aiff[..valid_aiff.len() - 1].to_vec();
        set_form_size(&mut aiff_truncated_padding);

        let mut wav_overflowing_chunk = riff_with_chunks([(*b"fmt ", pcm_format_body())]);
        wav_overflowing_chunk.extend_from_slice(b"data");
        wav_overflowing_chunk.extend_from_slice(&u32::MAX.to_le_bytes());
        set_riff_size(&mut wav_overflowing_chunk);

        let mut aiff_overflowing_chunk = form_with_chunks(b"AIFF", [(*b"COMM", vec![0; 18])]);
        aiff_overflowing_chunk.extend_from_slice(b"SSND");
        aiff_overflowing_chunk.extend_from_slice(&u32::MAX.to_be_bytes());
        set_form_size(&mut aiff_overflowing_chunk);

        let mut wav_short_fmt = riff_with_chunks([(*b"fmt ", vec![0; 15]), (*b"data", vec![1])]);
        let mut aiff_short_comm = form_with_chunks(
            b"AIFF",
            [(*b"COMM", vec![0; 17]), (*b"SSND", ssnd_body(0, &[1]))],
        );
        let mut aifc_short_comm = form_with_chunks(
            b"AIFC",
            [(*b"COMM", vec![0; 21]), (*b"SSND", ssnd_body(0, &[1]))],
        );
        set_riff_size(&mut wav_short_fmt);
        set_form_size(&mut aiff_short_comm);
        set_form_size(&mut aifc_short_comm);

        let cases = [
            (AudioFormat::Wav, b"RIFF".to_vec()),
            (AudioFormat::Aiff, b"FORM".to_vec()),
            (AudioFormat::Wav, wav_wrong_size),
            (AudioFormat::Aiff, aiff_wrong_size),
            (AudioFormat::Wav, riff_with_body(b"fmt ")),
            (AudioFormat::Aiff, form_with_body(b"AIFF", b"COMM")),
            (
                AudioFormat::Wav,
                riff_with_chunks([(*b"fmt ", pcm_format_body()), (*b"data", Vec::new())]),
            ),
            (
                AudioFormat::Aiff,
                form_with_chunks(b"AIFF", [(*b"COMM", vec![0; 18])]),
            ),
            (AudioFormat::Wav, wav_short_fmt),
            (AudioFormat::Aiff, aiff_short_comm),
            (AudioFormat::Aiff, aifc_short_comm),
            (AudioFormat::Wav, wav_truncated_padding),
            (AudioFormat::Aiff, aiff_truncated_padding),
            (AudioFormat::Wav, wav_overflowing_chunk),
            (AudioFormat::Aiff, aiff_overflowing_chunk),
            (
                AudioFormat::Aiff,
                form_with_chunks(b"AIFF", [(*b"COMM", vec![0; 18]), (*b"SSND", vec![0; 7])]),
            ),
            (
                AudioFormat::Aiff,
                form_with_chunks(
                    b"AIFF",
                    [(*b"COMM", vec![0; 18]), (*b"SSND", ssnd_body(2, &[1]))],
                ),
            ),
            (
                AudioFormat::Aiff,
                form_with_chunks(
                    b"AIFF",
                    [(*b"COMM", vec![0; 18]), (*b"SSND", ssnd_body(1, &[1]))],
                ),
            ),
        ];

        for (format, bytes) in cases {
            let error = SynthesizedAudio::new(bytes, format).validate().unwrap_err();
            assert_eq!(error.message(), INVALID_CONTAINER);
        }

        assert_eq!(
            SynthesizedAudio::new(b"RIFF".to_vec(), AudioFormat::Wav)
                .validate()
                .unwrap_err()
                .message(),
            INVALID_CONTAINER
        );
        assert_eq!(
            SynthesizedAudio::new(b"FORM".to_vec(), AudioFormat::Aiff)
                .validate()
                .unwrap_err()
                .message(),
            INVALID_CONTAINER
        );
    }

    fn minimal_wav() -> Vec<u8> {
        riff_with_chunks([(*b"fmt ", pcm_format_body()), (*b"data", vec![1])])
    }

    fn pcm_format_body() -> Vec<u8> {
        [
            1_u16.to_le_bytes().as_slice(),
            1_u16.to_le_bytes().as_slice(),
            16_000_u32.to_le_bytes().as_slice(),
            32_000_u32.to_le_bytes().as_slice(),
            2_u16.to_le_bytes().as_slice(),
            16_u16.to_le_bytes().as_slice(),
        ]
        .concat()
    }

    fn minimal_aiff(form_type: &[u8; 4], comm_size: usize) -> Vec<u8> {
        form_with_chunks(
            form_type,
            [
                (*b"COMM", vec![0; comm_size]),
                (*b"SSND", ssnd_body(0, &[1])),
            ],
        )
    }

    fn ssnd_body(offset: u32, sound_data: &[u8]) -> Vec<u8> {
        [
            offset.to_be_bytes().as_slice(),
            0_u32.to_be_bytes().as_slice(),
            sound_data,
        ]
        .concat()
    }

    fn riff_with_body(body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::from(&b"RIFF"[..]);
        bytes.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_le_bytes());
        bytes.extend_from_slice(b"WAVE");
        bytes.extend_from_slice(body);
        bytes
    }

    fn riff_with_chunks<const N: usize>(chunks: [([u8; 4], Vec<u8>); N]) -> Vec<u8> {
        let mut body = Vec::from(&b"WAVE"[..]);
        for (chunk_id, chunk_body) in chunks {
            append_chunk(&mut body, &chunk_id, &chunk_body, u32::to_le_bytes);
        }
        riff_with_body(&body[4..])
    }

    fn form_with_body(form_type: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::from(&b"FORM"[..]);
        bytes.extend_from_slice(&u32::try_from(body.len() + 4).unwrap().to_be_bytes());
        bytes.extend_from_slice(form_type);
        bytes.extend_from_slice(body);
        bytes
    }

    fn form_with_chunks<const N: usize>(
        form_type: &[u8; 4],
        chunks: [([u8; 4], Vec<u8>); N],
    ) -> Vec<u8> {
        let mut body = Vec::new();
        for (chunk_id, chunk_body) in chunks {
            append_chunk(&mut body, &chunk_id, &chunk_body, u32::to_be_bytes);
        }
        form_with_body(form_type, &body)
    }

    fn append_chunk(
        bytes: &mut Vec<u8>,
        chunk_id: &[u8; 4],
        chunk_body: &[u8],
        encode_size: fn(u32) -> [u8; 4],
    ) {
        bytes.extend_from_slice(chunk_id);
        bytes.extend_from_slice(&encode_size(u32::try_from(chunk_body.len()).unwrap()));
        bytes.extend_from_slice(chunk_body);
        if chunk_body.len() % 2 == 1 {
            bytes.push(0);
        }
    }

    fn set_riff_size(bytes: &mut [u8]) {
        let size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
    }

    fn set_form_size(bytes: &mut [u8]) {
        let size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&size.to_be_bytes());
    }
}
