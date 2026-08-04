use std::fmt;
use std::io;

use conversation_protocol::MAX_CLIENT_FRAME_BYTES;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    InvalidLength(usize),
    Truncated,
}

impl fmt::Display for FrameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "frame I/O failed: {error}"),
            Self::InvalidLength(length) => write!(
                formatter,
                "frame length must be within 1..={MAX_CLIENT_FRAME_BYTES}, got {length}"
            ),
            Self::Truncated => formatter.write_str("framed input ended before a complete frame"),
        }
    }
}

impl std::error::Error for FrameError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::InvalidLength(_) | Self::Truncated => None,
        }
    }
}

pub struct FrameReader<R> {
    reader: R,
}

impl<R> FrameReader<R>
where
    R: AsyncRead + Unpin,
{
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    pub async fn read_frame(&mut self) -> Result<Option<Vec<u8>>, FrameError> {
        let mut header = [0_u8; 4];
        if !read_complete(&mut self.reader, &mut header).await? {
            return Ok(None);
        }
        let length = u32::from_be_bytes(header) as usize;
        validate_length(length)?;

        let mut payload = vec![0_u8; length];
        if !read_complete(&mut self.reader, &mut payload).await? {
            return Err(FrameError::Truncated);
        }
        Ok(Some(payload))
    }
}

pub struct FrameWriter<W> {
    writer: W,
}

impl<W> FrameWriter<W>
where
    W: AsyncWrite + Unpin,
{
    pub fn new(writer: W) -> Self {
        Self { writer }
    }

    pub async fn write_frame(&mut self, payload: &[u8]) -> Result<(), FrameError> {
        validate_length(payload.len())?;
        let length = u32::try_from(payload.len()).expect("frame length is capped at 512 KiB");
        self.writer
            .write_all(&length.to_be_bytes())
            .await
            .map_err(FrameError::Io)?;
        self.writer
            .write_all(payload)
            .await
            .map_err(FrameError::Io)?;
        self.writer.flush().await.map_err(FrameError::Io)
    }
}

async fn read_complete<R>(reader: &mut R, bytes: &mut [u8]) -> Result<bool, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut offset = 0;
    while offset < bytes.len() {
        let count = reader
            .read(&mut bytes[offset..])
            .await
            .map_err(FrameError::Io)?;
        if count == 0 {
            return if offset == 0 {
                Ok(false)
            } else {
                Err(FrameError::Truncated)
            };
        }
        offset += count;
    }
    Ok(true)
}

fn validate_length(length: usize) -> Result<(), FrameError> {
    if (1..=MAX_CLIENT_FRAME_BYTES).contains(&length) {
        Ok(())
    } else {
        Err(FrameError::InvalidLength(length))
    }
}
