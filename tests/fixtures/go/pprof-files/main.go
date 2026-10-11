// Writes two profiles of its own with runtime/pprof, whose contents are
// exact: every allocation of allocate's (1000 of 64 bytes) in the first,
// and three goroutines parked under a label in the second.
package main

import (
	"context"
	"os"
	"runtime"
	"runtime/pprof"
	"sync"
)

var kept [][]byte

//go:noinline
func allocate() {
	for range 1000 {
		kept = append(kept, make([]byte, 64))
	}
}

func park(done chan struct{}, parked *sync.WaitGroup) {
	parked.Done()
	<-done
}

func write(name, path string) {
	file, err := os.Create(path)
	if err != nil {
		panic(err)
	}
	if err := pprof.Lookup(name).WriteTo(file, 0); err != nil {
		panic(err)
	}
	if err := file.Close(); err != nil {
		panic(err)
	}
}

func main() {
	runtime.MemProfileRate = 1
	kept = make([][]byte, 0, 1000)
	allocate()
	// A heap profile shows allocations as of the last collection.
	runtime.GC()
	write("allocs", os.Args[1])

	done := make(chan struct{})
	var parked sync.WaitGroup
	parked.Add(3)
	labels := pprof.Labels("worker", "parked")
	pprof.Do(context.Background(), labels, func(context.Context) {
		for range 3 {
			go park(done, &parked)
		}
	})
	parked.Wait()
	write("goroutine", os.Args[2])
	close(done)
}
