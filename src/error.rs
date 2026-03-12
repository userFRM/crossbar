//! Error types for the crossbar crate.
//!
//! All fallible operations in crossbar return [`CrossbarError`], which captures
//! the distinct failure modes that can occur across the transport and
//! serialization layers.

use std::fmt;

// ── CrossbarError ────────────────────────────────────────

/// The unified error type for all crossbar operations.
///
/// Returned by [`UdsClient`](crate::transport::UdsClient),
/// [`TcpClient`](crate::transport::TcpClient), and
/// [`ChannelClient`](crate::transport::ChannelClient) methods.
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

    /// The server-side task was dropped before the request could be processed.
    ///
    /// Typically indicates the [`ChannelServer`](crate::transport::ChannelServer)
    /// task has exited.
    ServerDropped,

    /// The connection was closed by the remote before a response was received.
    ConnectionClosed,

    /// A peer sent a frame larger than [`MAX_FRAME_SIZE`](crate::transport::MAX_FRAME_SIZE).
    FrameTooLarge {
        /// The declared frame size.
        size: usize,
        /// The configured maximum.
        max: usize,
    },
}

impl fmt::Display for CrossbarError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CrossbarError::Io(e) => write!(f, "I/O error: {e}"),
            CrossbarError::Serialize(e) => write!(f, "serialization error: {e}"),
            CrossbarError::Deserialize(e) => write!(f, "deserialization error: {e}"),
            CrossbarError::InvalidMethod(b) => write!(f, "invalid method byte: 0x{b:02x}"),
            CrossbarError::ServerDropped => {
                write!(f, "server dropped: the router task has exited")
            }
            CrossbarError::ConnectionClosed => {
                write!(f, "connection closed by remote peer")
            }
            CrossbarError::FrameTooLarge { size, max } => {
                write!(f, "frame too large: {size} bytes (max {max})")
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
