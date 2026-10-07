// Workers that run the same function at once while everything that makes
// run control hard happens around them: each yields every round, so the
// scheduler moves it between threads; its calls grow its stack; it sends
// the process SIGURG, as the runtime's preemption does; and a collector
// runs again and again, shrinking stacks it scans. The program checks its
// own work and says so.
package main

import (
	"fmt"
	"os"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
)

const (
	workers = 8
	rounds  = 200
)

// sink keeps allocations the compiler would otherwise drop.
var sink []byte

// deep recurses with a large frame, so a goroutine's stack grows the first
// time it reaches each new depth.
//
//go:noinline
func deep(depth int) int {
	var pad [256]int
	pad[depth%len(pad)] = depth
	if depth == 0 {
		return pad[0]
	}
	return deep(depth-1) + pad[depth%len(pad)]
}

// step is one round of a worker's work.
//
//go:noinline
func step(id, round int, total *int) {
	*total += deep(round % 32) // TORTURE: step
	*total ^= id               // TORTURE: mix
}

//go:noinline
func work(id int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		step(id, round, &total) // TORTURE: call
		runtime.Gosched()
		if round%5 == 0 {
			syscall.Kill(os.Getpid(), syscall.SIGURG)
		}
	}
	return total
}

// expected is what work computes, without the program's goroutines.
func expected(id int) int {
	total := 0
	for round := 0; round < rounds; round++ {
		depth := round % 32
		total += depth * (depth + 1) / 2
		total ^= id
	}
	return total
}

func main() {
	var stop atomic.Bool
	collected := make(chan struct{})
	go func() {
		defer close(collected)
		for !stop.Load() {
			sink = make([]byte, 64<<10)
			runtime.GC()
		}
	}()

	var group sync.WaitGroup
	totals := make([]int, workers)
	for id := range workers {
		group.Add(1)
		go func() {
			defer group.Done()
			totals[id] = work(id)
		}()
	}
	group.Wait()
	stop.Store(true)
	<-collected
	for id, total := range totals {
		if total != expected(id) {
			fmt.Printf("worker %d computed %d, not %d\n", id, total, expected(id))
			os.Exit(1)
		}
	}
	fmt.Println("every worker's work is right")
}
