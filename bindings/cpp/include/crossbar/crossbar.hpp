// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

/// @file crossbar.hpp
/// @brief Modern C++17 RAII bindings for the crossbar zero-copy IPC library.
///
/// This header wraps the C FFI surface (`crossbar.h`) with move-only RAII
/// classes. All resources are cleaned up automatically on scope exit.
/// Null returns from C functions throw `crossbar::Error`.
///
/// Usage:
/// @code
///   #include <crossbar/crossbar.hpp>
///
///   crossbar::PoolPublisher pub("prices");
///   auto topic = pub.register_topic("/tick/AAPL");
///   auto loan  = pub.loan(topic);
///   loan.write(&price, sizeof(price));
///   loan.publish();
/// @endcode

#ifndef CROSSBAR_HPP_
#define CROSSBAR_HPP_

#include "crossbar.h"

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <optional>
#include <stdexcept>
#include <utility>

namespace crossbar {

// ---- Error ----------------------------------------------------------------

/// Exception thrown when a C FFI call returns null.
///
/// The message is populated from `crossbar_last_error()` when available,
/// falling back to a generic description.
class Error : public std::runtime_error {
public:
    explicit Error(const char* what) : std::runtime_error(what) {}

    /// Construct from `crossbar_last_error()`, falling back to @p fallback.
    static Error from_last(const char* fallback) {
        const char* msg = crossbar_last_error();
        return Error(msg ? msg : fallback);
    }
};

// ---- Config ---------------------------------------------------------------

/// Publisher configuration mirroring `crossbar_config_t`.
///
/// Default values match `crossbar_config_default()`.
struct Config {
    uint32_t max_topics       = 16;
    uint32_t block_count      = 256;
    uint32_t block_size       = 65536;
    uint32_t ring_depth       = 8;
    uint64_t heartbeat_ms     = 100;
    uint64_t stale_timeout_ms = 5000;

    /// Convert to the C representation.
    crossbar_config_t to_c() const noexcept {
        crossbar_config_t c{};
        c.max_topics       = max_topics;
        c.block_count      = block_count;
        c.block_size       = block_size;
        c.ring_depth       = ring_depth;
        c.heartbeat_ms     = heartbeat_ms;
        c.stale_timeout_ms = stale_timeout_ms;
        return c;
    }
};

// ---- Loan -----------------------------------------------------------------

/// RAII wrapper around a `crossbar_loan_t*` — a mutable view into a shared
/// memory pool block.
///
/// Write data into the loan, then call `publish()` to make it visible to
/// subscribers. If the loan is destroyed without publishing, the underlying
/// block is returned to the pool automatically.
///
/// Move-only. Not copyable.
class Loan {
public:
    /// Takes ownership of a raw loan pointer. @p p must not be null.
    explicit Loan(crossbar_loan_t* p) noexcept : ptr_(p) {}

    ~Loan() {
        if (ptr_) crossbar_loan_drop(ptr_);
    }

    Loan(Loan&& o) noexcept : ptr_(std::exchange(o.ptr_, nullptr)) {}

    Loan& operator=(Loan&& o) noexcept {
        if (this != &o) {
            if (ptr_) crossbar_loan_drop(ptr_);
            ptr_ = std::exchange(o.ptr_, nullptr);
        }
        return *this;
    }

    Loan(const Loan&) = delete;
    Loan& operator=(const Loan&) = delete;

    /// Pointer to the writable data region in shared memory.
    uint8_t* data() noexcept { return crossbar_loan_data(ptr_); }

    /// Maximum number of bytes this loan can hold.
    size_t capacity() const noexcept { return crossbar_loan_capacity(ptr_); }

    /// Set the valid data length after writing via `data()`.
    void set_len(size_t n) noexcept { crossbar_loan_set_len(ptr_, n); }

    /// Copy @p n bytes from @p src into the loan and set the length.
    ///
    /// @pre @p n <= capacity()
    void write(const void* src, size_t n) {
        std::memcpy(data(), src, n);
        set_len(n);
    }

    /// Publish the loan (O(1) transfer) and release ownership.
    ///
    /// After this call the Loan is moved-from and must not be used.
    void publish() {
        crossbar_loan_publish(ptr_);
        ptr_ = nullptr;
    }

    /// Check whether this loan still owns a block.
    explicit operator bool() const noexcept { return ptr_ != nullptr; }

private:
    crossbar_loan_t* ptr_;
};

// ---- TopicHandle ----------------------------------------------------------

/// RAII wrapper around a `crossbar_topic_t*` — identifies a registered topic.
///
/// Returned by `PoolPublisher::register_topic()`. Pass to
/// `PoolPublisher::loan()` to obtain a writable block for that topic.
///
/// Move-only. Not copyable.
class TopicHandle {
public:
    explicit TopicHandle(crossbar_topic_t* p) noexcept : ptr_(p) {}

    ~TopicHandle() {
        if (ptr_) crossbar_topic_destroy(ptr_);
    }

    TopicHandle(TopicHandle&& o) noexcept : ptr_(std::exchange(o.ptr_, nullptr)) {}

    TopicHandle& operator=(TopicHandle&& o) noexcept {
        if (this != &o) {
            if (ptr_) crossbar_topic_destroy(ptr_);
            ptr_ = std::exchange(o.ptr_, nullptr);
        }
        return *this;
    }

    TopicHandle(const TopicHandle&) = delete;
    TopicHandle& operator=(const TopicHandle&) = delete;

    /// Access the underlying C handle (for passing to C FFI functions).
    const crossbar_topic_t* get() const noexcept { return ptr_; }

    explicit operator bool() const noexcept { return ptr_ != nullptr; }

private:
    crossbar_topic_t* ptr_;
};

// ---- Sample ---------------------------------------------------------------

/// RAII wrapper around a `crossbar_sample_t*` — a zero-copy reference to
/// published data in shared memory.
///
/// The underlying pool block is held alive by atomic refcounting until this
/// object is destroyed. Reading `data()` is safe without additional
/// synchronization.
///
/// Move-only. Not copyable.
class Sample {
public:
    explicit Sample(crossbar_sample_t* p) noexcept : ptr_(p) {}

    ~Sample() {
        if (ptr_) crossbar_sample_drop(ptr_);
    }

    Sample(Sample&& o) noexcept : ptr_(std::exchange(o.ptr_, nullptr)) {}

    Sample& operator=(Sample&& o) noexcept {
        if (this != &o) {
            if (ptr_) crossbar_sample_drop(ptr_);
            ptr_ = std::exchange(o.ptr_, nullptr);
        }
        return *this;
    }

    Sample(const Sample&) = delete;
    Sample& operator=(const Sample&) = delete;

    /// Pointer to the immutable sample data in shared memory.
    const uint8_t* data() const noexcept { return crossbar_sample_data(ptr_); }

    /// Number of valid bytes in the sample.
    size_t size() const noexcept { return crossbar_sample_len(ptr_); }

    /// Alias for `size()` (STL convention).
    size_t length() const noexcept { return size(); }

    /// True if the sample has zero length.
    bool empty() const noexcept { return size() == 0; }

    /// Element access (no bounds checking).
    const uint8_t& operator[](size_t i) const noexcept { return data()[i]; }

    /// Span-like begin iterator.
    const uint8_t* begin() const noexcept { return data(); }

    /// Span-like end iterator.
    const uint8_t* end() const noexcept { return data() + size(); }

    explicit operator bool() const noexcept { return ptr_ != nullptr; }

private:
    crossbar_sample_t* ptr_;
};

// ---- Subscription ---------------------------------------------------------

/// RAII wrapper around a `crossbar_subscription_t*` — a subscription to a
/// single topic.
///
/// Call `try_recv()` in a poll loop to receive published samples.
///
/// Move-only. Not copyable.
class Subscription {
public:
    explicit Subscription(crossbar_subscription_t* p) noexcept : ptr_(p) {}

    ~Subscription() {
        if (ptr_) crossbar_subscription_destroy(ptr_);
    }

    Subscription(Subscription&& o) noexcept : ptr_(std::exchange(o.ptr_, nullptr)) {}

    Subscription& operator=(Subscription&& o) noexcept {
        if (this != &o) {
            if (ptr_) crossbar_subscription_destroy(ptr_);
            ptr_ = std::exchange(o.ptr_, nullptr);
        }
        return *this;
    }

    Subscription(const Subscription&) = delete;
    Subscription& operator=(const Subscription&) = delete;

    /// Non-blocking receive. Returns the next sample or `std::nullopt`.
    std::optional<Sample> try_recv() {
        crossbar_sample_t* s = crossbar_subscription_try_recv(ptr_);
        if (!s) return std::nullopt;
        return Sample(s);
    }

    explicit operator bool() const noexcept { return ptr_ != nullptr; }

private:
    crossbar_subscription_t* ptr_;
};

// ---- PoolPublisher --------------------------------------------------------

/// RAII wrapper around `crossbar_publisher_t*` — creates a shared memory
/// pub/sub region and publishes data via zero-copy loans.
///
/// # Example
///
/// @code
///   crossbar::Config cfg;
///   cfg.max_topics = 4;
///
///   crossbar::PoolPublisher pub("prices", cfg);
///   auto topic = pub.register_topic("/tick/AAPL");
///
///   auto loan = pub.loan(topic);
///   double price = 182.35;
///   loan.write(&price, sizeof(price));
///   loan.publish();
/// @endcode
///
/// Move-only. Not copyable.
class PoolPublisher {
public:
    /// Create a publisher with the given name and configuration.
    ///
    /// @param name  Region name (maps to `/dev/shm/crossbar-pool-<name>`).
    /// @param cfg   Publisher configuration (defaults are usually fine).
    /// @throws crossbar::Error if the region cannot be created.
    explicit PoolPublisher(const char* name, const Config& cfg = {}) {
        crossbar_config_t c = cfg.to_c();
        ptr_ = crossbar_publisher_create(name, &c);
        if (!ptr_) throw Error::from_last("failed to create publisher");
    }

    ~PoolPublisher() {
        if (ptr_) crossbar_publisher_destroy(ptr_);
    }

    PoolPublisher(PoolPublisher&& o) noexcept : ptr_(std::exchange(o.ptr_, nullptr)) {}

    PoolPublisher& operator=(PoolPublisher&& o) noexcept {
        if (this != &o) {
            if (ptr_) crossbar_publisher_destroy(ptr_);
            ptr_ = std::exchange(o.ptr_, nullptr);
        }
        return *this;
    }

    PoolPublisher(const PoolPublisher&) = delete;
    PoolPublisher& operator=(const PoolPublisher&) = delete;

    /// Register a topic URI. Returns a handle for use with `loan()`.
    ///
    /// @param topic  Topic URI (e.g. "/tick/AAPL"). Max 64 bytes.
    /// @throws crossbar::Error if max topics reached or URI too long.
    TopicHandle register_topic(const char* topic) {
        crossbar_topic_t* t = crossbar_publisher_register(ptr_, topic);
        if (!t) throw Error::from_last("failed to register topic");
        return TopicHandle(t);
    }

    /// Loan a block from the pool for writing to @p handle's topic.
    ///
    /// Write data into the returned `Loan`, then call `Loan::publish()`.
    ///
    /// @throws crossbar::Error if the pool is exhausted.
    Loan loan(const TopicHandle& handle) {
        crossbar_loan_t* l = crossbar_publisher_loan(ptr_, handle.get());
        if (!l) throw Error::from_last("failed to obtain loan (pool exhausted?)");
        return Loan(l);
    }

    explicit operator bool() const noexcept { return ptr_ != nullptr; }

private:
    crossbar_publisher_t* ptr_;
};

// ---- PoolSubscriber -------------------------------------------------------

/// RAII wrapper around `crossbar_subscriber_t*` — connects to an existing
/// pub/sub region and subscribes to topics.
///
/// # Example
///
/// @code
///   crossbar::PoolSubscriber sub("prices");
///   auto stream = sub.subscribe("/tick/AAPL");
///
///   while (auto sample = stream.try_recv()) {
///       double price;
///       std::memcpy(&price, sample->data(), sizeof(price));
///       std::printf("price: %.2f\n", price);
///   }
/// @endcode
///
/// Move-only. Not copyable.
class PoolSubscriber {
public:
    /// Connect to an existing pub/sub region.
    ///
    /// @param name  Region name (must match the publisher's name).
    /// @throws crossbar::Error if the region does not exist, has an invalid
    ///         header, or the publisher heartbeat is stale.
    explicit PoolSubscriber(const char* name) {
        ptr_ = crossbar_subscriber_connect(name);
        if (!ptr_) throw Error::from_last("failed to connect subscriber");
    }

    ~PoolSubscriber() {
        if (ptr_) crossbar_subscriber_destroy(ptr_);
    }

    PoolSubscriber(PoolSubscriber&& o) noexcept : ptr_(std::exchange(o.ptr_, nullptr)) {}

    PoolSubscriber& operator=(PoolSubscriber&& o) noexcept {
        if (this != &o) {
            if (ptr_) crossbar_subscriber_destroy(ptr_);
            ptr_ = std::exchange(o.ptr_, nullptr);
        }
        return *this;
    }

    PoolSubscriber(const PoolSubscriber&) = delete;
    PoolSubscriber& operator=(const PoolSubscriber&) = delete;

    /// Subscribe to a topic by URI.
    ///
    /// @param topic  Topic URI (must have been registered by the publisher).
    /// @throws crossbar::Error if the topic is not found.
    Subscription subscribe(const char* topic) {
        crossbar_subscription_t* s = crossbar_subscriber_subscribe(ptr_, topic);
        if (!s) throw Error::from_last("failed to subscribe to topic");
        return Subscription(s);
    }

    explicit operator bool() const noexcept { return ptr_ != nullptr; }

private:
    crossbar_subscriber_t* ptr_;
};

} // namespace crossbar

#endif // CROSSBAR_HPP_
