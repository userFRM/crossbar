/* Copyright (c) 2026 The Crossbar Contributors
 *
 * This source code is licensed under the MIT license or Apache License 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
 * root for details.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

/**
 * @file crossbar.h
 * @brief C FFI for crossbar pool-backed zero-copy pub/sub over shared memory.
 *
 * ## Ownership rules
 *
 * - Every `_create` / `_connect` must be paired with `_destroy`.
 * - Every `_loan` must be consumed by `crossbar_loan_publish` or `crossbar_loan_drop`.
 * - All loans MUST be published or dropped before calling `crossbar_publisher_destroy`.
 * - `crossbar_loan_publish` and `crossbar_sample_drop` consume their pointer argument;
 *   do not use the pointer after calling them.
 * - Functions return NULL on error; call `crossbar_last_error()` for details.
 */

#ifndef CROSSBAR_H
#define CROSSBAR_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Opaque handle types ─────────────────────────────────────────────── */

typedef struct crossbar_publisher_t crossbar_publisher_t;
typedef struct crossbar_topic_t crossbar_topic_t;
typedef struct crossbar_loan_t crossbar_loan_t;
typedef struct crossbar_subscriber_t crossbar_subscriber_t;
typedef struct crossbar_subscription_t crossbar_subscription_t;
typedef struct crossbar_sample_t crossbar_sample_t;

/* ── Config ──────────────────────────────────────────────────────────── */

typedef struct {
    uint32_t max_topics;
    uint32_t block_count;
    uint32_t block_size;
    uint32_t ring_depth;
    uint64_t heartbeat_interval_us;
    uint64_t stale_timeout_us;
} crossbar_config_t;

/**
 * Returns a config struct populated with default values.
 */
crossbar_config_t crossbar_config_default(void);

/* ── Publisher ────────────────────────────────────────────────────────── */

/**
 * Creates a pool publisher for the named shared-memory region.
 * Returns NULL on error.
 */
crossbar_publisher_t* crossbar_publisher_create(const char* name,
                                                 const crossbar_config_t* config);

/**
 * Destroys a publisher. All loans must have been published/dropped first.
 */
void crossbar_publisher_destroy(crossbar_publisher_t* pub_);

/**
 * Registers a topic URI. Returns a handle or NULL on error.
 */
crossbar_topic_t* crossbar_publisher_register(crossbar_publisher_t* pub_,
                                               const char* topic);

/**
 * Destroys a topic handle.
 */
void crossbar_topic_destroy(crossbar_topic_t* topic);

/**
 * Loans a pool block for writing. Returns NULL if the pool is exhausted.
 * The loan must be consumed via crossbar_loan_publish or crossbar_loan_drop
 * before destroying the publisher.
 */
crossbar_loan_t* crossbar_publisher_loan(crossbar_publisher_t* pub_,
                                          const crossbar_topic_t* handle);

/**
 * Returns a mutable pointer to the loan's data buffer.
 */
uint8_t* crossbar_loan_data(crossbar_loan_t* loan);

/**
 * Returns the maximum number of bytes the loan can hold.
 */
size_t crossbar_loan_capacity(const crossbar_loan_t* loan);

/**
 * Sets the valid data length. Must not exceed capacity.
 */
void crossbar_loan_set_len(crossbar_loan_t* loan, size_t len);

/**
 * Publishes the loan and wakes subscribers. Consumes the loan pointer.
 */
void crossbar_loan_publish(crossbar_loan_t* loan);

/**
 * Drops the loan without publishing. Consumes the loan pointer.
 */
void crossbar_loan_drop(crossbar_loan_t* loan);

/* ── Subscriber ───────────────────────────────────────────────────────── */

/**
 * Connects to an existing pool publisher region. Returns NULL on error.
 */
crossbar_subscriber_t* crossbar_subscriber_connect(const char* name);

/**
 * Destroys a subscriber.
 */
void crossbar_subscriber_destroy(crossbar_subscriber_t* sub);

/**
 * Subscribes to a topic by URI. Returns NULL if the topic is not found.
 */
crossbar_subscription_t* crossbar_subscriber_subscribe(crossbar_subscriber_t* sub,
                                                        const char* topic);

/**
 * Destroys a subscription.
 */
void crossbar_subscription_destroy(crossbar_subscription_t* sub);

/**
 * Non-blocking receive. Returns a sample guard or NULL if no new data.
 */
crossbar_sample_t* crossbar_subscription_try_recv(crossbar_subscription_t* sub);

/**
 * Returns a pointer to the sample's immutable data.
 */
const uint8_t* crossbar_sample_data(const crossbar_sample_t* sample);

/**
 * Returns the sample's data length in bytes.
 */
size_t crossbar_sample_len(const crossbar_sample_t* sample);

/**
 * Drops a sample guard, releasing the pool block. Consumes the pointer.
 */
void crossbar_sample_drop(crossbar_sample_t* sample);

/* ── Error ────────────────────────────────────────────────────────────── */

/**
 * Returns the last error message for the calling thread, or an empty
 * string if no error has occurred. The pointer is valid until the next
 * FFI call on the same thread.
 */
const char* crossbar_last_error(void);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* CROSSBAR_H */
