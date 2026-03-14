/*
 * Copyright (c) 2026 The Crossbar Contributors
 *
 * This source code is licensed under the MIT license or Apache License 2.0,
 * at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
 * root for details.
 *
 * SPDX-License-Identifier: MIT OR Apache-2.0
 */

#ifndef CROSSBAR_H
#define CROSSBAR_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* ── Configuration ────────────────────────────────────────────────────── */

typedef struct {
    uint32_t max_topics;
    uint32_t block_count;
    uint32_t block_size;
    uint32_t ring_depth;
    uint64_t heartbeat_ms;
    uint64_t stale_timeout_ms;
} crossbar_config_t;

crossbar_config_t crossbar_config_default(void);

/* ── Opaque handles ───────────────────────────────────────────────────── */

typedef struct crossbar_publisher_t crossbar_publisher_t;
typedef struct crossbar_topic_t crossbar_topic_t;
typedef struct crossbar_loan_t crossbar_loan_t;
typedef struct crossbar_subscriber_t crossbar_subscriber_t;
typedef struct crossbar_subscription_t crossbar_subscription_t;
typedef struct crossbar_sample_t crossbar_sample_t;

/* ── Publisher ─────────────────────────────────────────────────────────── */

crossbar_publisher_t* crossbar_publisher_create(const char* name,
                                                const crossbar_config_t* config);
void crossbar_publisher_destroy(crossbar_publisher_t* pub);

crossbar_topic_t* crossbar_publisher_register(crossbar_publisher_t* pub,
                                              const char* topic);
void crossbar_topic_destroy(crossbar_topic_t* topic);

crossbar_loan_t* crossbar_publisher_loan(crossbar_publisher_t* pub,
                                         const crossbar_topic_t* handle);
uint8_t* crossbar_loan_data(crossbar_loan_t* loan);
size_t crossbar_loan_capacity(const crossbar_loan_t* loan);
void crossbar_loan_set_len(crossbar_loan_t* loan, size_t len);
void crossbar_loan_publish(crossbar_loan_t* loan);
void crossbar_loan_drop(crossbar_loan_t* loan);

/* ── Subscriber ────────────────────────────────────────────────────────── */

crossbar_subscriber_t* crossbar_subscriber_connect(const char* name);
void crossbar_subscriber_destroy(crossbar_subscriber_t* sub);

crossbar_subscription_t* crossbar_subscriber_subscribe(crossbar_subscriber_t* sub,
                                                       const char* topic);
void crossbar_subscription_destroy(crossbar_subscription_t* sub);

crossbar_sample_t* crossbar_subscription_try_recv(crossbar_subscription_t* sub);

const uint8_t* crossbar_sample_data(const crossbar_sample_t* sample);
size_t crossbar_sample_len(const crossbar_sample_t* sample);
void crossbar_sample_drop(crossbar_sample_t* sample);

/* ── Error ─────────────────────────────────────────────────────────────── */

const char* crossbar_last_error(void);

#ifdef __cplusplus
}
#endif

#endif /* CROSSBAR_H */
