// A goroutine locked to its thread spins in a loop with no calls, while
// another collects garbage without end. Each collection stops the world,
// which the spinning goroutine reaches only when the runtime preempts it
// with SIGURG, so the runtime keeps sending SIGURG to the spinning thread.
// Were one delivered while the debugger steps that thread alone, the
// goroutine would park until the collector's stopped thread restarts the
// world, and the step would never end.
package main

import (
	"fmt"
	"runtime"
)

// sink keeps calls the compiler would otherwise drop.
var sink int

// spin loops without calls, so only a signal can preempt it.
//
//go:noinline
func spin(rounds int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		total += round ^ (total >> 3) // the loop
	}
	return total
}

func main() {
	runtime.LockOSThread()
	go func() {
		for {
			runtime.GC()
		}
	}()
	fmt.Println("spinning")
	for {
		sink += spin(1 << 20)
	}
}
