// Goroutines whose calls into the runtime continue on a thread's system
// stack, each the way one of the runtime's stack switches makes it:
// `systemstack` runs a function there and returns, `morestack` grows a
// goroutine's stack and resumes it, and `mcall` parks a goroutine.
// Tests stop in the runtime on the system stack and unwind back onto the
// goroutine's own.
package main

import (
	"fmt"
	"runtime"
	"strings"
)

// sink keeps calls the compiler would otherwise drop.
var sink int

// stats reads the memory statistics, which the runtime gathers on the
// system stack with the world stopped.
//
//go:noinline
func stats() {
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	sink += int(memory.NumGC)
}

// grow recurses deep enough that its goroutine's stack must grow.
//
//go:noinline
func grow(depth int) int {
	var pad [64]int
	pad[depth%64] = depth
	if depth == 0 {
		return pad[0]
	}
	return grow(depth-1) + pad[depth%64]
}

// awaitParked yields until the runtime's dump shows a goroutine that
// began in `function` waiting in `status`.
func awaitParked(function, status string) {
	buffer := make([]byte, 1<<16)
	for {
		dump := string(buffer[:runtime.Stack(buffer, true)])
		for _, block := range strings.Split(dump, "\n\n") {
			if strings.Contains(block, "["+status) && strings.Contains(block, function+"(") {
				return
			}
		}
		runtime.Gosched()
	}
}

func main() {
	stats()
	// A goroutine that parks once, and stays parked.
	never := make(chan int)
	go func() {
		<-never
	}()
	done := make(chan int)
	go func() {
		done <- grow(2000)
	}()
	// Main parks until the result arrives.
	sink += <-done
	awaitParked("main.main.func1", "chan receive")
	fmt.Println(sink)
}
