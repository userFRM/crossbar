/*
 * crossbar — Zero-copy pub/sub over shared memory.
 *
 * C API. Link against libcrossbar.so / libcrossbar.dylib / crossbar.dll.
 * Build: cargo build --release --features ffi
 *
 * Apache-2.0
 */

#ifndef CROSSBAR_H
#define CROSSBAR_H

#include <stdint.h>
#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ---- Opaque types ---- */

typedef struct ShmPublisher crossbar_publisher_t;
typedef struct ShmSubscriber crossbar_subscriber_t;
typedef struct Subscription crossbar_subscription_t;
typedef struct CrossbarSample crossbar_sample_t;
typedef struct ShmChannel crossbar_channel_t;

/* ---- Value types ---- */

typedef struct {
    uint32_t topic_idx;
    uint64_t publisher_id;
} crossbar_topic_t;

typedef struct {
    uint32_t max_topics;        /* default: 16    */
    uint32_t block_count;       /* default: 256   */
    uint32_t block_size;        /* default: 65536 */
    uint32_t ring_depth;        /* default: 8     */
    uint64_t heartbeat_ms;      /* default: 100   */
    uint64_t stale_timeout_ms;  /* default: 5000  */
} crossbar_config_t;

/* Returns a default configuration. */
crossbar_config_t crossbar_config_default(void);

/* ---- Publisher ---- */

/* Creates a publisher. Returns NULL on error. */
crossbar_publisher_t* crossbar_publisher_create(
    const char* name,
    const crossbar_config_t* config  /* NULL for defaults */
);

/* Frees a publisher. All loans must be published/freed first. */
void crossbar_publisher_free(crossbar_publisher_t* pub_);

/* Registers a topic URI. Returns topic handle; topic_idx == UINT32_MAX on error. */
crossbar_topic_t crossbar_publisher_register(
    crossbar_publisher_t* pub_,
    const char* uri
);

/* Updates publisher heartbeat. Call during idle periods.
   Returns 0 on success, -1 on clock error. */
int crossbar_publisher_heartbeat(crossbar_publisher_t* pub_);

/* Copies data into a SHM block and publishes. Returns 0 on success. */
int crossbar_publish(
    crossbar_publisher_t* pub_,
    crossbar_topic_t topic,
    const void* data,
    size_t len
);

/* Returns the number of active subscribers for a topic. Returns 0 if pub_ is NULL. */
uint32_t crossbar_topic_subscriber_count(crossbar_publisher_t* pub_, crossbar_topic_t topic);

/* ---- Subscriber ---- */

/* Connects to an existing publisher region. Returns NULL on error. */
crossbar_subscriber_t* crossbar_subscriber_connect(const char* name);

/* Frees a subscriber. All subscriptions must be freed first. */
void crossbar_subscriber_free(crossbar_subscriber_t* sub);

/* Subscribes to a topic by URI. Returns NULL on error. */
crossbar_subscription_t* crossbar_subscriber_subscribe(
    crossbar_subscriber_t* sub,
    const char* uri
);

/* Frees a subscription. All samples must be freed first. */
void crossbar_subscription_free(crossbar_subscription_t* stream);

/* ---- Sample (zero-copy) ---- */

/* Non-blocking receive. Returns NULL if no new data. Allocates per call. */
crossbar_sample_t* crossbar_try_recv(crossbar_subscription_t* stream);

/* Non-blocking receive into caller-provided memory (zero allocation).
 * Returns 1 if sample was written to out, 0 if no data.
 * Call crossbar_sample_free(out) when done if this returns 1. */
int crossbar_try_recv_into(crossbar_subscription_t* stream, crossbar_sample_t* out);

/* Blocking receive. Returns NULL on error (publisher dead). */
crossbar_sample_t* crossbar_recv(crossbar_subscription_t* stream);

/* Returns pointer to sample data (zero-copy — points directly into SHM). */
const uint8_t* crossbar_sample_data(const crossbar_sample_t* sample);

/* Returns sample data length in bytes. */
size_t crossbar_sample_len(const crossbar_sample_t* sample);

/* Frees a sample (decrements block refcount; returns to pool if last ref). */
void crossbar_sample_free(crossbar_sample_t* sample);

/* ---- Channel (bidirectional) ---- */

/* Creates server side. Blocks up to timeout_ms for client. Returns NULL on error. */
crossbar_channel_t* crossbar_channel_listen(
    const char* name,
    const crossbar_config_t* config,  /* NULL for defaults */
    uint64_t timeout_ms
);

/* Creates client side. Retries up to timeout_ms for server. Returns NULL on error. */
crossbar_channel_t* crossbar_channel_connect(
    const char* name,
    const crossbar_config_t* config,  /* NULL for defaults */
    uint64_t timeout_ms
);

/* Frees a channel. */
void crossbar_channel_free(crossbar_channel_t* ch);

/* Sends data through a channel. Returns 0 on success. */
int crossbar_channel_send(crossbar_channel_t* ch, const void* data, size_t len);

/* Non-blocking receive. Returns NULL if no data. */
crossbar_sample_t* crossbar_channel_try_recv(crossbar_channel_t* ch);

/* Blocking receive. Returns NULL on error. */
crossbar_sample_t* crossbar_channel_recv(crossbar_channel_t* ch);

#ifdef __cplusplus
}
#endif

#endif /* CROSSBAR_H */
