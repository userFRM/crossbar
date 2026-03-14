// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

// Publisher example: writes 1000 price ticks to a shared memory topic.
//
// Build the FFI library first:
//
//	cargo build --release -p crossbar-ffi
//
// Then run:
//
//	CGO_LDFLAGS="-L$VIADUCT_ROOT/target/release" \
//	LD_LIBRARY_PATH="$VIADUCT_ROOT/target/release" \
//	go run ./examples/publisher
package main

import (
	"encoding/binary"
	"fmt"
	"log"
	"math"
	"time"

	"github.com/userFRM/crossbar/bindings/go/crossbar"
)

func main() {
	cfg := crossbar.DefaultConfig()
	pub, err := crossbar.NewPoolPublisher("market", cfg)
	if err != nil {
		log.Fatalf("failed to create publisher: %v", err)
	}
	defer pub.Close()

	topic, err := pub.Register("prices/AAPL")
	if err != nil {
		log.Fatalf("failed to register topic: %v", err)
	}
	defer topic.Close()

	fmt.Println("publishing 1000 price ticks on prices/AAPL ...")

	for i := 0; i < 1000; i++ {
		loan, err := pub.Loan(topic)
		if err != nil {
			log.Fatalf("failed to obtain loan: %v", err)
		}

		price := 150.0 + float64(i)*0.01
		binary.LittleEndian.PutUint64(loan.Data(), math.Float64bits(price))
		loan.SetLen(8)
		loan.Publish()

		if i%100 == 0 {
			fmt.Printf("  tick %d: AAPL = %.2f\n", i, price)
		}

		time.Sleep(10 * time.Millisecond)
	}

	fmt.Println("done.")
}
