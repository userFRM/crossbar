// Copyright (c) 2026 The Crossbar Contributors
//
// This source code is licensed under the MIT license or Apache License 2.0,
// at your option. See LICENSE-MIT and LICENSE-APACHE files in the project
// root for details.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

// Subscriber example: reads price ticks from a shared memory topic.
//
// Start the publisher example first, then run this in another terminal:
//
//	CGO_LDFLAGS="-L$VIADUCT_ROOT/target/release" \
//	LD_LIBRARY_PATH="$VIADUCT_ROOT/target/release" \
//	go run ./examples/subscriber
package main

import (
	"encoding/binary"
	"fmt"
	"log"
	"math"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/userFRM/crossbar/bindings/go/crossbar"
)

func main() {
	sub, err := crossbar.NewPoolSubscriber("market")
	if err != nil {
		log.Fatalf("failed to connect subscriber: %v", err)
	}
	defer sub.Close()

	stream, err := sub.Subscribe("prices/AAPL")
	if err != nil {
		log.Fatalf("failed to subscribe: %v", err)
	}
	defer stream.Close()

	fmt.Println("subscribed to prices/AAPL, waiting for ticks ...")

	// Graceful shutdown on Ctrl-C.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	count := 0
	for {
		select {
		case <-sigCh:
			fmt.Printf("\nreceived %d samples, shutting down.\n", count)
			return
		default:
		}

		if sample, ok := stream.TryRecv(); ok {
			data := sample.Data()
			if len(data) >= 8 {
				price := math.Float64frombits(binary.LittleEndian.Uint64(data))
				fmt.Printf("AAPL: %.2f\n", price)
			}
			sample.Close()
			count++
		} else {
			time.Sleep(time.Millisecond)
		}
	}
}
