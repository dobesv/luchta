//! Postcard codec for cache metadata and cache-key inputs.

use serde::{Deserialize, Serialize};

/// Error encoding cache metadata. Keeps the serializer out of the public API.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EncodeError(postcard::Error);

#[derive(Debug, thiserror::Error)]
pub(crate) enum DecodeError {
    #[error("{0}")]
    Invalid(#[from] postcard::Error),
    #[error("trailing bytes after cache metadata")]
    TrailingBytes,
}

pub(crate) fn encode<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, EncodeError> {
    postcard::to_allocvec(value).map_err(EncodeError)
}

pub(crate) fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T, DecodeError> {
    let (value, remaining) = postcard::take_from_bytes(bytes)?;
    if !remaining.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(value)
}
