// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Transport adapters: in-process and shared memory.
//!
//! Crossbar supports two transport families:
//!
//! **Request/Response** — a server dispatches incoming requests through a
//! [`Router`](crate::router::Router), a client sends [`Request`]s and
//! receives [`Response`]s.
//!
//! | Transport | Typical latency | Notes |
//! |-----------|-----------------|-------|
//! | [`InProcessClient`] | ~143 ns | Direct dispatch, no serialization |
//! | `PubSubRpcClient` | ~561 ns | Pub/sub-backed RPC, 25% faster than slot-based |
//! | `ShmClient` | ~745 ns | Cross-process via `/dev/shm` (`shm` feature) |
//!
//! **Pub/Sub** — zero-copy O(1) streaming over shared memory.
//!
//! | Transport | Latency | Notes |
//! |-----------|---------|-------|
//! | `ShmPoolPublisher` / `ShmPoolSubscriber` | 67 ns | O(1) transfer, safe reads |

mod inproc;
#[cfg(all(unix, feature = "shm"))]
pub(crate) mod shm;

pub use inproc::InProcessClient;
#[cfg(all(unix, feature = "shm"))]
pub use shm::{
    BidiClient, BidiConfig, BidiServer, PoolPubSubConfig, PoolTopicHandle, PubSubConfig,
    PubSubRpcClient, PubSubRpcConfig, PubSubRpcServer, ShmClient, ShmConfig, ShmHandle, ShmLoan,
    ShmPoolLoan, ShmPoolPublisher, ShmPoolSampleGuard, ShmPoolSubscriber, ShmPoolSubscription,
    ShmPublisher, ShmSample, ShmSampleRef, ShmServer, ShmSubscriber, ShmSubscription, TopicHandle,
};

#[cfg(feature = "shm")]
use crate::error::CrossbarError;
#[cfg(feature = "shm")]
use std::collections::HashMap;
#[cfg(feature = "shm")]
use std::io;

// -- Headers serialization helpers --

/// Serialize headers directly into a target buffer (SHM block).
/// Returns the number of bytes written.
#[cfg(feature = "shm")]
#[allow(clippy::cast_possible_truncation)] // lengths verified <= u16::MAX before cast
pub(crate) fn serialize_headers_into(
    headers: &HashMap<String, String>,
    target: &mut [u8],
) -> Result<usize, CrossbarError> {
    if headers.len() > u16::MAX as usize {
        return Err(CrossbarError::HeaderOverflow(format!(
            "header count {} exceeds u16::MAX",
            headers.len()
        )));
    }
    let num = headers.len() as u16;
    let mut pos = 0;

    if pos + 2 > target.len() {
        return Err(CrossbarError::ShmMessageTooLarge {
            size: 2,
            max: target.len(),
        });
    }
    target[pos..pos + 2].copy_from_slice(&num.to_le_bytes());
    pos += 2;

    for (k, v) in headers {
        let kb = k.as_bytes();
        let vb = v.as_bytes();
        if kb.len() > u16::MAX as usize {
            return Err(CrossbarError::HeaderOverflow(format!(
                "header key length {} exceeds u16::MAX",
                kb.len()
            )));
        }
        if vb.len() > u16::MAX as usize {
            return Err(CrossbarError::HeaderOverflow(format!(
                "header value length {} exceeds u16::MAX",
                vb.len()
            )));
        }
        let needed = 2 + kb.len() + 2 + vb.len();
        if pos + needed > target.len() {
            return Err(CrossbarError::ShmMessageTooLarge {
                size: pos + needed,
                max: target.len(),
            });
        }
        target[pos..pos + 2].copy_from_slice(&(kb.len() as u16).to_le_bytes());
        pos += 2;
        target[pos..pos + kb.len()].copy_from_slice(kb);
        pos += kb.len();
        target[pos..pos + 2].copy_from_slice(&(vb.len() as u16).to_le_bytes());
        pos += 2;
        target[pos..pos + vb.len()].copy_from_slice(vb);
        pos += vb.len();
    }
    Ok(pos)
}

/// Deserializes a header map from the wire format.
#[cfg(feature = "shm")]
pub(crate) fn deserialize_headers(data: &[u8]) -> Result<HashMap<String, String>, CrossbarError> {
    if data.len() < 2 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "headers data too short").into());
    }
    let num = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut pos = 2;
    let mut headers = HashMap::with_capacity(num);

    for _ in 0..num {
        if pos + 2 > data.len() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "truncated header key length").into(),
            );
        }
        let key_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + key_len > data.len() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated header key").into());
        }
        let key = std::str::from_utf8(&data[pos..pos + key_len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header key not UTF-8"))?
            .to_string();
        pos += key_len;

        if pos + 2 > data.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "truncated header value length",
            )
            .into());
        }
        let val_len = u16::from_le_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2;

        if pos + val_len > data.len() {
            return Err(
                io::Error::new(io::ErrorKind::InvalidData, "truncated header value").into(),
            );
        }
        let val = std::str::from_utf8(&data[pos..pos + val_len])
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "header value not UTF-8"))?
            .to_string();
        pos += val_len;

        headers.insert(key, val);
    }

    Ok(headers)
}
