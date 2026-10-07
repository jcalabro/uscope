// Goroutines that run the same function at the same time, where a step
// must stay with the goroutine it began in. Siblings run the stepped lines
// meanwhile; each yield lets the scheduler resume the goroutine on another
// thread, or run a sibling on its own; and deep calls grow the goroutine's
// stack, which the runtime then copies somewhere else.
package main

import (
	"runtime"
	"strconv"
	"strings"
	"sync"
)

// sink keeps calls the compiler would otherwise drop.
var sink int

// me is the calling goroutine's id, from the header of its own dump.
func me() int {
	buffer := make([]byte, 64)
	header := string(buffer[:runtime.Stack(buffer, false)])
	id, _, _ := strings.Cut(strings.TrimPrefix(header, "goroutine "), " ")
	number, err := strconv.Atoi(id)
	if err != nil {
		panic(err)
	}
	return number
}

// deep recurses with a large frame, so a goroutine's stack grows the first
// time it reaches each new depth.
//
//go:noinline
func deep(depth int) int {
	var pad [512]int
	pad[depth%len(pad)] = depth
	if depth == 0 {
		return pad[0]
	}
	return deep(depth-1) + pad[depth%len(pad)] // recurse
}

// work is what every goroutine runs; `id` is the goroutine's own.
//
//go:noinline
func work(rounds int) int {
	id := me()
	// The goroutine's first deep call grows its fresh stack several times.
	total := deep(32)
	for round := 0; round < rounds; round++ { // WALK: loop
		total += round            // WALK: add
		runtime.Gosched()         // WALK: yield
		total += deep(round % 64) // WALK: deep
		total ^= id               // WALK: mix
	}
	return total + id
}

func main() {
	var group sync.WaitGroup
	for range 8 {
		group.Add(1)
		go func() {
			defer group.Done()
			total := work(10000)
			sink += total // returned
		}()
	}
	group.Wait()
}
