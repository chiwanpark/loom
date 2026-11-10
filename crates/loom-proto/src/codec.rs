use serde::Serialize;
use serde::de::DeserializeOwned;

pub const MAX_FRAME_LEN: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame of {0} bytes exceeds the {MAX_FRAME_LEN} byte limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(#[from] postcard::Error),
}

pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, CodecError> {
    let bytes = postcard::to_allocvec(message)?;
    if bytes.len() > MAX_FRAME_LEN {
        return Err(CodecError::TooLarge(bytes.len()));
    }
    Ok(bytes)
}

pub fn decode<T: DeserializeOwned>(frame: &[u8]) -> Result<T, CodecError> {
    if frame.len() > MAX_FRAME_LEN {
        return Err(CodecError::TooLarge(frame.len()));
    }
    Ok(postcard::from_bytes(frame)?)
}
