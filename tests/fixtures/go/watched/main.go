// A goroutine's local watched while the runtime moves its stack. Between
// the stores to the local, deep calls grow the goroutine's stack, which
// the runtime copies elsewhere each time, and sibling goroutines run the
// same function with locals of their own at the same depth.
package main

import "sync"

// sink keeps results the compiler would otherwise drop.
var sink int

// deep recurses with a large frame, so the stack grows the first time it
// reaches each new depth.
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

// bump stores to its caller's local, which stays on the caller's stack,
// since bump keeps no pointer to it.
//
//go:noinline
func bump(counter *int, by int) {
	*counter += by // WATCH: store
}

//go:noinline
func watched(rounds int) int {
	counter := 0
	for round := 1; round <= rounds; round++ { // WATCH: loop
		sink += deep(round * 16)
		bump(&counter, round)
	}
	return counter
}

// finished runs once every goroutine's watched has returned.
//
//go:noinline
func finished() {
	sink++
}

func main() {
	var group sync.WaitGroup
	for range 4 {
		group.Add(1)
		go func() {
			defer group.Done()
			sink += watched(4)
		}()
	}
	group.Wait()
	finished()
}
