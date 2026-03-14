// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

// Package crossbar provides Go bindings for the crossbar zero-copy IPC library.
//
// Crossbar uses shared memory and a pool-backed ring buffer to achieve O(1)
// zero-copy publish/subscribe. Publishers write data directly into shared
// memory blocks and transfer only an 8-byte descriptor (block index + length)
// to subscribers, regardless of payload size.
//
// # Publisher workflow
//
//	pub, err := crossbar.NewPoolPublisher("market", crossbar.DefaultConfig())
//	if err != nil { log.Fatal(err) }
//	defer pub.Close()
//
//	topic, err := pub.Register("prices/AAPL")
//	if err != nil { log.Fatal(err) }
//	defer topic.Close()
//
//	loan, err := pub.Loan(topic)
//	if err != nil { log.Fatal(err) }
//	loan.Write(payload)
//	loan.Publish()
//
// # Subscriber workflow
//
//	sub, err := crossbar.NewPoolSubscriber("market")
//	if err != nil { log.Fatal(err) }
//	defer sub.Close()
//
//	stream, err := sub.Subscribe("prices/AAPL")
//	if err != nil { log.Fatal(err) }
//	defer stream.Close()
//
//	if sample, ok := stream.TryRecv(); ok {
//	    data := sample.Data()
//	    // ... process data ...
//	    sample.Close()
//	}
package crossbar

// #cgo LDFLAGS: -lcrossbar_ffi
// #include "crossbar.h"
// #include <stdlib.h>
import "C"

import (
	"errors"
	"fmt"
	"unsafe"
)

// lastError retrieves the most recent error message from the C FFI layer.
// Returns a generic message if no error string is available.
func lastError() error {
	msg := C.crossbar_last_error()
	if msg != nil {
		return errors.New(C.GoString(msg))
	}
	return errors.New("crossbar: unknown error")
}

// ---------------------------------------------------------------------------
// PoolPublisher
// ---------------------------------------------------------------------------

// PoolPublisher creates and manages a pool-backed pub/sub shared memory region.
//
// Only one PoolPublisher may be active per named region at a time. The publisher
// owns the shared memory file and removes it on Close.
//
// PoolPublisher is not safe for concurrent use from multiple goroutines.
type PoolPublisher struct {
	ptr *C.crossbar_publisher_t
}

// NewPoolPublisher creates a new pool-backed pub/sub region with the given name
// and configuration.
//
// The name determines the shared memory file path (e.g., /dev/shm/crossbar-pool-<name>
// on Linux). Use [DefaultConfig] for sensible defaults.
//
// Returns an error if the region cannot be created or another publisher already
// holds the region lock.
func NewPoolPublisher(name string, cfg Config) (*PoolPublisher, error) {
	cname := C.CString(name)
	defer C.free(unsafe.Pointer(cname))

	ccfg := cfg.toC()
	ptr := C.crossbar_publisher_create(cname, &ccfg)
	if ptr == nil {
		return nil, fmt.Errorf("crossbar: failed to create publisher: %w", lastError())
	}
	return &PoolPublisher{ptr: ptr}, nil
}

// Register registers a topic URI with the publisher and returns a handle
// that can be used with [PoolPublisher.Loan].
//
// Topic URIs must be at most 64 bytes. Returns an error if the maximum number
// of topics has been reached or the URI is too long.
func (p *PoolPublisher) Register(topic string) (*TopicHandle, error) {
	if p.ptr == nil {
		return nil, errors.New("crossbar: publisher is closed")
	}

	ctopic := C.CString(topic)
	defer C.free(unsafe.Pointer(ctopic))

	ptr := C.crossbar_publisher_register(p.ptr, ctopic)
	if ptr == nil {
		return nil, fmt.Errorf("crossbar: failed to register topic %q: %w", topic, lastError())
	}
	return &TopicHandle{ptr: ptr}, nil
}

// Loan allocates a block from the shared pool for writing. Write your data
// into the returned [Loan], then call [Loan.Publish] to make it visible to
// subscribers or [Loan.Drop] to release it without publishing.
//
// Returns an error if the pool is exhausted (all blocks in use).
func (p *PoolPublisher) Loan(h *TopicHandle) (*Loan, error) {
	if p.ptr == nil {
		return nil, errors.New("crossbar: publisher is closed")
	}
	if h.ptr == nil {
		return nil, errors.New("crossbar: topic handle is closed")
	}

	ptr := C.crossbar_publisher_loan(p.ptr, h.ptr)
	if ptr == nil {
		return nil, fmt.Errorf("crossbar: failed to obtain loan: %w", lastError())
	}
	return &Loan{ptr: ptr}, nil
}

// Close destroys the publisher and releases the shared memory region.
// Close is idempotent.
func (p *PoolPublisher) Close() {
	if p.ptr != nil {
		C.crossbar_publisher_destroy(p.ptr)
		p.ptr = nil
	}
}

// ---------------------------------------------------------------------------
// TopicHandle
// ---------------------------------------------------------------------------

// TopicHandle identifies a registered topic for use with [PoolPublisher.Loan].
//
// Close the handle when it is no longer needed.
type TopicHandle struct {
	ptr *C.crossbar_topic_t
}

// Close releases the topic handle. Close is idempotent.
func (h *TopicHandle) Close() {
	if h.ptr != nil {
		C.crossbar_topic_destroy(h.ptr)
		h.ptr = nil
	}
}

// ---------------------------------------------------------------------------
// Loan
// ---------------------------------------------------------------------------

// Loan is a mutable view into a pool block in shared memory.
//
// Write data into it, then call [Loan.Publish] to transfer ownership to
// subscribers. The transfer is O(1) -- only 8 bytes are written to the ring,
// regardless of payload size.
//
// If neither Publish nor Drop is called, the block leaks until the publisher
// is destroyed.
type Loan struct {
	ptr *C.crossbar_loan_t
}

// Data returns a Go byte slice that points directly into shared memory.
//
// WARNING: The returned slice is only valid while the Loan is alive. Do not
// retain the slice after calling [Loan.Publish] or [Loan.Drop]. Writing to the
// slice modifies the shared memory block directly (zero-copy).
//
// The slice length equals [Loan.Capacity]. Use [Loan.SetLen] to declare how
// many bytes you actually wrote.
func (l *Loan) Data() []byte {
	if l.ptr == nil {
		return nil
	}
	dataPtr := C.crossbar_loan_data(l.ptr)
	cap := C.crossbar_loan_capacity(l.ptr)
	if dataPtr == nil || cap == 0 {
		return nil
	}
	return unsafe.Slice((*byte)(unsafe.Pointer(dataPtr)), int(cap))
}

// Capacity returns the maximum number of bytes this loan can hold.
func (l *Loan) Capacity() int {
	if l.ptr == nil {
		return 0
	}
	return int(C.crossbar_loan_capacity(l.ptr))
}

// SetLen declares the number of valid data bytes in the loan.
//
// Call this after writing data via [Loan.Data] and before [Loan.Publish].
// Panics if n exceeds [Loan.Capacity].
func (l *Loan) SetLen(n int) {
	if l.ptr == nil {
		return
	}
	C.crossbar_loan_set_len(l.ptr, C.size_t(n))
}

// Write copies data into the loan starting at offset 0 and sets the length.
//
// This is a convenience method equivalent to:
//
//	copy(loan.Data(), data)
//	loan.SetLen(len(data))
//
// Panics if len(data) exceeds [Loan.Capacity].
func (l *Loan) Write(data []byte) {
	buf := l.Data()
	if len(data) > len(buf) {
		panic(fmt.Sprintf("crossbar: data length %d exceeds loan capacity %d", len(data), len(buf)))
	}
	copy(buf, data)
	l.SetLen(len(data))
}

// Publish transfers the block to subscribers and consumes the loan.
//
// After calling Publish, the Loan must not be used. The transfer is O(1).
func (l *Loan) Publish() {
	if l.ptr != nil {
		C.crossbar_loan_publish(l.ptr)
		l.ptr = nil
	}
}

// Drop releases the block back to the pool without publishing it.
//
// After calling Drop, the Loan must not be used.
func (l *Loan) Drop() {
	if l.ptr != nil {
		C.crossbar_loan_drop(l.ptr)
		l.ptr = nil
	}
}

// ---------------------------------------------------------------------------
// PoolSubscriber
// ---------------------------------------------------------------------------

// PoolSubscriber connects to an existing publisher's shared memory region
// for zero-copy reading.
//
// PoolSubscriber is not safe for concurrent use from multiple goroutines.
type PoolSubscriber struct {
	ptr *C.crossbar_subscriber_t
}

// NewPoolSubscriber connects to an existing pool pub/sub region by name.
//
// Returns an error if the region does not exist, has an invalid format, or
// the publisher's heartbeat is stale.
func NewPoolSubscriber(name string) (*PoolSubscriber, error) {
	cname := C.CString(name)
	defer C.free(unsafe.Pointer(cname))

	ptr := C.crossbar_subscriber_connect(cname)
	if ptr == nil {
		return nil, fmt.Errorf("crossbar: failed to connect subscriber: %w", lastError())
	}
	return &PoolSubscriber{ptr: ptr}, nil
}

// Subscribe creates a subscription to the given topic URI.
//
// The topic must have been previously registered by the publisher. Returns an
// error if the topic is not found.
func (s *PoolSubscriber) Subscribe(topic string) (*Subscription, error) {
	if s.ptr == nil {
		return nil, errors.New("crossbar: subscriber is closed")
	}

	ctopic := C.CString(topic)
	defer C.free(unsafe.Pointer(ctopic))

	ptr := C.crossbar_subscriber_subscribe(s.ptr, ctopic)
	if ptr == nil {
		return nil, fmt.Errorf("crossbar: failed to subscribe to %q: %w", topic, lastError())
	}
	return &Subscription{ptr: ptr}, nil
}

// Close destroys the subscriber and unmaps the shared memory region.
// Close is idempotent.
func (s *PoolSubscriber) Close() {
	if s.ptr != nil {
		C.crossbar_subscriber_destroy(s.ptr)
		s.ptr = nil
	}
}

// ---------------------------------------------------------------------------
// Subscription
// ---------------------------------------------------------------------------

// Subscription receives samples from a single topic.
//
// Use [Subscription.TryRecv] for non-blocking polling.
type Subscription struct {
	ptr *C.crossbar_subscription_t
}

// TryRecv performs a non-blocking receive. Returns a [Sample] and true if new
// data is available, or nil and false otherwise.
//
// The caller must call [Sample.Close] when done to release the shared memory
// block back to the pool.
func (sub *Subscription) TryRecv() (*Sample, bool) {
	if sub.ptr == nil {
		return nil, false
	}

	ptr := C.crossbar_subscription_try_recv(sub.ptr)
	if ptr == nil {
		return nil, false
	}
	return &Sample{ptr: ptr}, true
}

// Close destroys the subscription. Close is idempotent.
func (sub *Subscription) Close() {
	if sub.ptr != nil {
		C.crossbar_subscription_destroy(sub.ptr)
		sub.ptr = nil
	}
}

// ---------------------------------------------------------------------------
// Sample
// ---------------------------------------------------------------------------

// Sample is a zero-copy reference to a published message in shared memory.
//
// The underlying shared memory block is held alive by an atomic refcount.
// You MUST call [Sample.Close] when finished reading to release the block
// back to the pool.
type Sample struct {
	ptr *C.crossbar_sample_t
}

// Data returns a Go byte slice that points directly into shared memory.
//
// WARNING: The returned slice is only valid while the Sample is alive. Do not
// retain the slice after calling [Sample.Close]. The data is read-only; writing
// to the slice results in undefined behavior.
func (s *Sample) Data() []byte {
	if s.ptr == nil {
		return nil
	}
	dataPtr := C.crossbar_sample_data(s.ptr)
	length := C.crossbar_sample_len(s.ptr)
	if dataPtr == nil || length == 0 {
		return nil
	}
	return unsafe.Slice((*byte)(unsafe.Pointer(dataPtr)), int(length))
}

// Len returns the number of valid data bytes in this sample.
func (s *Sample) Len() int {
	if s.ptr == nil {
		return 0
	}
	return int(C.crossbar_sample_len(s.ptr))
}

// Close releases the sample and decrements the shared memory block's refcount.
// When the refcount reaches zero, the block is returned to the pool.
//
// Close is idempotent.
func (s *Sample) Close() {
	if s.ptr != nil {
		C.crossbar_sample_drop(s.ptr)
		s.ptr = nil
	}
}
