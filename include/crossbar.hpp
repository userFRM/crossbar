/*
 * crossbar — Zero-copy pub/sub over shared memory.
 *
 * C++20 header-only wrapper over the C FFI (crossbar.h).
 * RAII wrappers with move semantics; exceptions on error.
 *
 * Apache-2.0
 */

#pragma once
#include "crossbar.h"
#include <cstdint>
#include <stdexcept>
#include <string>
#include <string_view>
#include <span>
#include <optional>

namespace crossbar {

// ---- Error ----------------------------------------------------------------

class Error : public std::runtime_error {
public:
    using std::runtime_error::runtime_error;
};

// ---- Sample (zero-copy) ---------------------------------------------------

class Sample {
    crossbar_sample_t* sample_;
public:
    explicit Sample(crossbar_sample_t* s) noexcept : sample_(s) {}
    ~Sample() { if (sample_) crossbar_sample_free(sample_); }

    // Move-only
    Sample(Sample&& o) noexcept : sample_(o.sample_) { o.sample_ = nullptr; }
    Sample& operator=(Sample&& o) noexcept {
        if (this != &o) {
            if (sample_) crossbar_sample_free(sample_);
            sample_ = o.sample_;
            o.sample_ = nullptr;
        }
        return *this;
    }
    Sample(const Sample&) = delete;
    Sample& operator=(const Sample&) = delete;

    const uint8_t* data() const noexcept { return crossbar_sample_data(sample_); }
    size_t size() const noexcept { return crossbar_sample_len(sample_); }
    std::span<const uint8_t> bytes() const noexcept { return {data(), size()}; }

    /// Reinterpret the sample payload as T. Throws if the sample is too small.
    template<typename T>
    const T& as() const {
        if (size() < sizeof(T)) throw Error("sample too small for type");
        return *reinterpret_cast<const T*>(data());
    }
};

// ---- Stream (subscription handle) -----------------------------------------

class Stream {
    crossbar_subscription_t* sub_;
public:
    explicit Stream(crossbar_subscription_t* s) noexcept : sub_(s) {}
    ~Stream() { if (sub_) crossbar_subscription_free(sub_); }

    Stream(Stream&& o) noexcept : sub_(o.sub_) { o.sub_ = nullptr; }
    Stream& operator=(Stream&& o) noexcept {
        if (this != &o) {
            if (sub_) crossbar_subscription_free(sub_);
            sub_ = o.sub_;
            o.sub_ = nullptr;
        }
        return *this;
    }
    Stream(const Stream&) = delete;
    Stream& operator=(const Stream&) = delete;

    /// Non-blocking receive. Returns std::nullopt if no data is available.
    std::optional<Sample> try_recv() {
        auto* s = crossbar_try_recv(sub_);
        if (!s) return std::nullopt;
        return Sample(s);
    }

    /// Blocking receive. Throws if the publisher is dead.
    Sample recv() {
        auto* s = crossbar_recv(sub_);
        if (!s) throw Error("publisher dead");
        return Sample(s);
    }
};

// ---- Subscriber -----------------------------------------------------------

class Subscriber {
    crossbar_subscriber_t* sub_;
public:
    explicit Subscriber(const std::string& name) {
        sub_ = crossbar_subscriber_connect(name.c_str());
        if (!sub_) throw Error("failed to connect: " + name);
    }
    ~Subscriber() { if (sub_) crossbar_subscriber_free(sub_); }

    Subscriber(Subscriber&& o) noexcept : sub_(o.sub_) { o.sub_ = nullptr; }
    Subscriber& operator=(Subscriber&& o) noexcept {
        if (this != &o) {
            if (sub_) crossbar_subscriber_free(sub_);
            sub_ = o.sub_;
            o.sub_ = nullptr;
        }
        return *this;
    }
    Subscriber(const Subscriber&) = delete;
    Subscriber& operator=(const Subscriber&) = delete;

    /// Subscribe to a topic URI. Throws if the topic is not found.
    Stream subscribe(const std::string& uri) {
        auto* s = crossbar_subscriber_subscribe(sub_, uri.c_str());
        if (!s) throw Error("topic not found: " + uri);
        return Stream(s);
    }
};

// ---- Topic ----------------------------------------------------------------

class Topic {
    crossbar_topic_t topic_;
public:
    explicit Topic(crossbar_topic_t t) : topic_(t) {
        if (t.topic_idx == UINT32_MAX) throw Error("failed to register topic");
    }
    crossbar_topic_t raw() const noexcept { return topic_; }
};

// ---- Publisher ------------------------------------------------------------

class Publisher {
    crossbar_publisher_t* pub_;
public:
    explicit Publisher(const std::string& name,
                       const crossbar_config_t& config = crossbar_config_default()) {
        pub_ = crossbar_publisher_create(name.c_str(), &config);
        if (!pub_) throw Error("failed to create publisher: " + name);
    }
    ~Publisher() { if (pub_) crossbar_publisher_free(pub_); }

    Publisher(Publisher&& o) noexcept : pub_(o.pub_) { o.pub_ = nullptr; }
    Publisher& operator=(Publisher&& o) noexcept {
        if (this != &o) {
            if (pub_) crossbar_publisher_free(pub_);
            pub_ = o.pub_;
            o.pub_ = nullptr;
        }
        return *this;
    }
    Publisher(const Publisher&) = delete;
    Publisher& operator=(const Publisher&) = delete;

    /// Register a topic URI. Throws on failure.
    Topic register_topic(const std::string& uri) {
        return Topic(crossbar_publisher_register(pub_, uri.c_str()));
    }

    /// Publish raw bytes to a topic. Throws on failure.
    void publish(const Topic& topic, const void* data, size_t len) {
        if (crossbar_publish(pub_, topic.raw(), data, len) != 0)
            throw Error("publish failed");
    }

    /// Publish a trivially-copyable value to a topic. Throws on failure.
    template<typename T>
    void publish(const Topic& topic, const T& value) {
        publish(topic, &value, sizeof(T));
    }

    /// Send heartbeat. Throws on clock error.
    void heartbeat() {
        if (crossbar_publisher_heartbeat(pub_) != 0)
            throw Error("heartbeat failed");
    }

    /// Returns the number of active subscribers for a topic.
    uint32_t subscriber_count(const Topic& topic) const noexcept {
        return crossbar_topic_subscriber_count(pub_, topic.raw());
    }
};

// ---- Channel (bidirectional) ----------------------------------------------

class Channel {
    crossbar_channel_t* ch_;

    explicit Channel(crossbar_channel_t* ch) noexcept : ch_(ch) {}
public:
    /// Create server side. Blocks up to timeout_ms for a client. Throws on error.
    static Channel listen(const std::string& name, uint64_t timeout_ms,
                          const crossbar_config_t& config = crossbar_config_default()) {
        auto* ch = crossbar_channel_listen(name.c_str(), &config, timeout_ms);
        if (!ch) throw Error("channel listen failed: " + name);
        return Channel(ch);
    }

    /// Create client side. Retries up to timeout_ms for a server. Throws on error.
    static Channel connect(const std::string& name, uint64_t timeout_ms,
                           const crossbar_config_t& config = crossbar_config_default()) {
        auto* ch = crossbar_channel_connect(name.c_str(), &config, timeout_ms);
        if (!ch) throw Error("channel connect failed: " + name);
        return Channel(ch);
    }

    ~Channel() { if (ch_) crossbar_channel_free(ch_); }

    Channel(Channel&& o) noexcept : ch_(o.ch_) { o.ch_ = nullptr; }
    Channel& operator=(Channel&& o) noexcept {
        if (this != &o) {
            if (ch_) crossbar_channel_free(ch_);
            ch_ = o.ch_;
            o.ch_ = nullptr;
        }
        return *this;
    }
    Channel(const Channel&) = delete;
    Channel& operator=(const Channel&) = delete;

    /// Send raw bytes. Throws on failure.
    void send(const void* data, size_t len) {
        if (crossbar_channel_send(ch_, data, len) != 0)
            throw Error("channel send failed");
    }

    /// Send a string_view. Throws on failure.
    void send(std::string_view msg) { send(msg.data(), msg.size()); }

    /// Send a trivially-copyable value. Throws on failure.
    template<typename T>
    void send(const T& value) { send(&value, sizeof(T)); }

    /// Non-blocking receive. Returns std::nullopt if no data.
    std::optional<Sample> try_recv() {
        auto* s = crossbar_channel_try_recv(ch_);
        if (!s) return std::nullopt;
        return Sample(s);
    }

    /// Blocking receive. Throws on error.
    Sample recv() {
        auto* s = crossbar_channel_recv(ch_);
        if (!s) throw Error("channel recv failed");
        return Sample(s);
    }
};

} // namespace crossbar
