// Workers park, one of them under profiler labels, and then main panics
// without recovering. Under GOTRACEBACK=crash the runtime prints every
// goroutine, its own among them, and aborts, leaving a core whose
// goroutines tests compare with that dump.
package main

import (
	"context"
	"runtime"
	"runtime/pprof"
	"strings"
)

// worker squares nothing: it waits for jobs that never come.
func worker(jobs <-chan int) {
	for range jobs {
	}
}

//go:noinline
func explode(reason string) {
	panic(reason)
}

func main() {
	jobs := make(chan int)
	for range 4 {
		go worker(jobs)
	}
	labels := pprof.Labels("job", "resize", "tenant", "a b")
	go pprof.Do(context.Background(), labels, func(context.Context) {
		worker(jobs)
	})
	// The dump shows each worker waiting once it has parked.
	buffer := make([]byte, 1<<16)
	for strings.Count(string(buffer[:runtime.Stack(buffer, true)]), "[chan receive") < 5 {
		runtime.Gosched()
	}
	explode("the workers are waiting")
}
