// Lines whose work goes through Go's runtime. Stepping in stops only in
// code the program wrote: it passes over the runtime's own machinery, the
// wrappers the compiler generates, and the stack check that grows a
// goroutine's stack. A step into a function stops at its declaration, as
// Go's line table ends the prologue there.
package main

// sink keeps calls the compiler would otherwise drop.
var sink int

type shape interface {
	area() int
}

type square struct {
	side int
}

//go:noinline
func (s square) area() int { // STEP: area
	return s.side * s.side
}

// fresh has a frame too large for a new goroutine's stack, so its
// prologue's stack check calls into the runtime to grow the stack.
//
//go:noinline
func fresh(seed int) int { // STEP: fresh
	var pad [4096]int
	pad[seed%len(pad)] = seed
	return pad[seed%len(pad)]
}

//go:noinline
func spawned(done chan<- int) { // STEP: spawned
	done <- 1 // STEP: send
}

//go:noinline
func run(done chan int) {
	values := map[string]int{}
	values["a"] = 1              // STEP: map
	var list []int               // STEP: list
	list = append(list, 1, 2, 3) // STEP: append
	var s shape = square{side: 3} // STEP: shape
	sink += s.area()         // STEP: interface
	go spawned(done)         // STEP: go
	sink += fresh(len(list)) // STEP: grow
	sink += values["a"]      // STEP: after
	<-done
}

func main() {
	done := make(chan int)
	finished := make(chan struct{})
	// run begins on a new goroutine's small stack.
	go func() { // STEP: start
		run(done) // STEP: body
		close(finished)
	}()
	<-finished
}
