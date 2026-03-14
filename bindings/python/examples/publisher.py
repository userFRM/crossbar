#!/usr/bin/env python3
# Copyright (c) 2026 The Crossbar Contributors
#
# This source code is licensed under the MIT license or Apache License 2.0,
# at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
# root for details.
#
# SPDX-License-Identifier: MIT OR Apache-2.0

"""Example publisher: streams 1000 price ticks at 10 ms intervals."""

import struct
import time

import crossbar

pub = crossbar.PoolPublisher("market")
topic = pub.register("prices/AAPL")

print("Publishing 1000 ticks to prices/AAPL ...")

for i in range(1000):
    loan = pub.loan(topic)
    loan.write(struct.pack("<d", 150.0 + i * 0.01))
    loan.publish()
    time.sleep(0.01)

print("Done.")
