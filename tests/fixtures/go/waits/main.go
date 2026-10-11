// Goroutines that wait where a profiler can see them: three park on a
// channel receive in receive, and one spins in spin, labeled phase=spin,
// until a byte on standard input releases the program, which a test sends
// once it has seen the program's tasks.
package main

import (
	"context"
	"os"
	"runtime/pprof"
	"sync"
	"sync/atomic"
)

const receivers = 3

// sink keeps work the compiler would otherwise drop.
var sink int

//go:noinline
func receive(values chan int, group *sync.WaitGroup) {
	defer group.Done()
	sink += <-values
}

//go:noinline
func spin(stop *atomic.Bool, done chan struct{}) {
	pprof.SetGoroutineLabels(pprof.WithLabels(context.Background(), pprof.Labels("phase", "spin")))
	for !stop.Load() {
		sink++
	}
	close(done)
}

func main() {
	values := make(chan int)
	var group sync.WaitGroup
	for index := 0; index < receivers; index++ {
		group.Add(1)
		go receive(values, &group)
	}
	var stop atomic.Bool
	done := make(chan struct{})
	go spin(&stop, done)
	release := make([]byte, 1)
	if _, err := os.Stdin.Read(release); err != nil {
		os.Exit(2)
	}
	stop.Store(true)
	<-done
	for index := 0; index < receivers; index++ {
		values <- index
	}
	group.Wait()
}
