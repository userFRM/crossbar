#!/usr/bin/env python3
# Copyright (c) 2026 The Crossbar Contributors
#
# This source code is licensed under the MIT license or Apache License 2.0,
# at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
# root for details.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

"""Example subscriber: reads price ticks published by publisher.py."""

import struct

import crossbar

sub = crossbar.PoolSubscriber("market")
stream = sub.subscribe("prices/AAPL")

print("Listening for prices/AAPL ...")

while True:
    try:
        data = stream.recv(timeout=5.0)
    except TimeoutError:
        print("No data for 5s, exiting.")
        break

    price = struct.unpack("<d", data[:8])[0]
    print(f"AAPL: {price:.2f}")
