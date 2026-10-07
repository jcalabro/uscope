// A program with a hundred thousand parked goroutines, which says how
// many goroutines the runtime counts once every one of them is parked.
package main

import (
	"fmt"
	"runtime"
	"sync"
)

const parked = 100_000

//go:noinline
func park(started *sync.WaitGroup, forever <-chan struct{}) {
	started.Done()
	<-forever // SCALE: parked
}

//go:noinline
func checkpoint() {}

func main() {
	var started sync.WaitGroup
	forever := make(chan struct{})
	started.Add(parked)
	for range parked {
		go park(&started, forever)
	}
	started.Wait()
	fmt.Println("goroutines", runtime.NumGoroutine())
	checkpoint()
}
