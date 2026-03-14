// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

package crossbar

// #include "crossbar.h"
import "C"

// Config holds the configuration for a pool-backed pub/sub region.
//
// All fields have sensible defaults obtainable via [DefaultConfig].
type Config struct {
	// MaxTopics is the maximum number of topics the region supports (default: 16).
	MaxTopics uint32

	// BlockCount is the number of blocks in the shared pool (default: 256).
	BlockCount uint32

	// BlockSize is the size of each block in bytes (default: 65536 = 64 KiB).
	// Usable data capacity is BlockSize - 8 (8 bytes for internal header).
	BlockSize uint32

	// RingDepth is how many published samples the ring remembers per topic
	// before overwriting older entries (default: 8).
	RingDepth uint32

	// HeartbeatMs is the publisher heartbeat interval in milliseconds (default: 100).
	HeartbeatMs uint64

	// StaleTimeoutMs is the duration in milliseconds after which a publisher
	// is considered dead if no heartbeat is received (default: 5000).
	StaleTimeoutMs uint64
}

// DefaultConfig returns the default configuration matching the C library defaults.
func DefaultConfig() Config {
	cc := C.crossbar_config_default()
	return Config{
		MaxTopics:      uint32(cc.max_topics),
		BlockCount:     uint32(cc.block_count),
		BlockSize:      uint32(cc.block_size),
		RingDepth:      uint32(cc.ring_depth),
		HeartbeatMs:    uint64(cc.heartbeat_ms),
		StaleTimeoutMs: uint64(cc.stale_timeout_ms),
	}
}

// toC converts a Go Config to the C crossbar_config_t.
func (c *Config) toC() C.crossbar_config_t {
	return C.crossbar_config_t{
		max_topics:      C.uint32_t(c.MaxTopics),
		block_count:     C.uint32_t(c.BlockCount),
		block_size:      C.uint32_t(c.BlockSize),
		ring_depth:      C.uint32_t(c.RingDepth),
		heartbeat_ms:    C.uint64_t(c.HeartbeatMs),
		stale_timeout_ms: C.uint64_t(c.StaleTimeoutMs),
	}
}
