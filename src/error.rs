//! Error types for the crossbar crate.
//!
//! All fallible operations in crossbar return [`CrossbarError`], which captures
//! the distinct failure modes that can occur across the transport and
//! serialization layers.

use std::fmt;

// -- CrossbarError ----

/// The unified error type for all crossbar operations.
///
/// # Examples
///
/// ```rust
/// use crossbar::error::CrossbarError;
///
/// fn show(e: CrossbarError) {
///     eprintln!("crossbar error: {e}");
/// }
/// ```
#[derive(Debug)]
pub enum CrossbarError {
    /// An I/O error occurred on the underlying stream or socket.
    Io(std::io::Error),

    /// A value could not be serialized to the wire format.
    Serialize(serde_json::Error),

    /// The received bytes could not be deserialized into the expected type.
    Deserialize(serde_json::Error),

    /// The method byte received over the wire did not correspond to a known
    /// [`Method`](crate::types::Method) variant.
    InvalidMethod(u8),

    /// The shared-memory server has stopped updating its heartbeat.
    #[cfg(feature = "shm")]
    ShmServerDead,

    /// The request payload exceeds the shared-memory block capacity.
    #[cfg(feature = "shm")]
    ShmMessageTooLarge {
        /// Total request size in bytes.
        size: usize,
        /// Configured block data capacity.
        max: usize,
    },

    /// All shared-memory slots are in use.
    #[cfg(feature = "shm")]
    ShmSlotsFull,

    /// All blocks in the shared-memory pool are in use.
    #[cfg(feature = "shm")]
    ShmPoolExhausted,

    /// The shared-memory region has invalid magic, version, or metadata.
    #[cfg(feature = "shm")]
    ShmInvalidRegion(String),

    /// A header key, value, or count exceeds the `u16` limit of the wire protocol.
    HeaderOverflow(String),
}

impl fmt::Display for CrossbarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrossbarError::Io(e) => write!(f, "I/O error: {e}"),
            CrossbarError::Serialize(e) => write!(f, "serialization error: {e}"),
            CrossbarError::Deserialize(e) => write!(f, "deserialization error: {e}"),
            CrossbarError::InvalidMethod(b) => write!(f, "invalid method byte: 0x{b:02x}"),
            #[cfg(feature = "shm")]
            CrossbarError::ShmServerDead => {
                write!(f, "shared-memory server is dead (heartbeat stale)")
            }
            #[cfg(feature = "shm")]
            CrossbarError::ShmMessageTooLarge { size, max } => {
                write!(
                    f,
                    "shared-memory message too large: {size} bytes (block capacity {max})"
                )
            }
            #[cfg(feature = "shm")]
            CrossbarError::ShmSlotsFull => {
                write!(f, "all shared-memory slots are in use")
            }
            #[cfg(feature = "shm")]
            CrossbarError::ShmPoolExhausted => {
                write!(f, "all shared-memory pool blocks are in use")
            }
            #[cfg(feature = "shm")]
            CrossbarError::ShmInvalidRegion(msg) => {
                write!(f, "invalid shared-memory region: {msg}")
            }
            CrossbarError::HeaderOverflow(msg) => {
                write!(f, "header overflow: {msg}")
            }
        }
    }
}

impl std::error::Error for CrossbarError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CrossbarError::Io(e) => Some(e),
            CrossbarError::Serialize(e) | CrossbarError::Deserialize(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for CrossbarError {
    #[inline]
    fn from(e: std::io::Error) -> Self {
        CrossbarError::Io(e)
    }
}
