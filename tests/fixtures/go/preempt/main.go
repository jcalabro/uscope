// Goroutines that compute until the Go runtime has preempted them
// asynchronously, which it does by sending the thread SIGURG, several times.
package main

import (
	"os"
	"os/signal"
	"sync"
	"sync/atomic"
	"syscall"
)

// How many SIGURG signals the program receives before it finishes.
const preemptions = 8

//go:noinline
func spin(count int) int {
	total := 0
	for index := 0; index < count; index++ {
		total += index % 7
	}
	return total
}

func main() {
	// The runtime passes its preemption signals on to the program too.
	urgent := make(chan os.Signal, 1)
	signal.Notify(urgent, syscall.SIGURG)
	var received atomic.Int64
	go func() {
		for range urgent {
			received.Add(1)
		}
	}()

	expected := spin(1_000_000)
	var group sync.WaitGroup
	var failed atomic.Bool
	for range 4 {
		group.Add(1)
		go func() {
			defer group.Done()
			for received.Load() < preemptions {
				if spin(1_000_000) != expected {
					failed.Store(true)
				}
			}
		}()
	}
	group.Wait()
	if failed.Load() {
		os.Exit(1)
	}
}
