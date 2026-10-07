// Deferred calls, which steps follow as the program runs them: the
// compiler calls a function's few defers itself as it returns, the runtime
// calls those registered in a loop, and a panic calls them all as it
// unwinds, until one recovers. Unoptimized, the compiler calls none
// itself, and the runtime calls them from deferreturn at a function's
// closing brace.
package main

// sink keeps calls the compiler would otherwise drop.
var sink int

//go:noinline
func cleanup(note int) { // DEFER: cleanup
	sink += note // DEFER: cleanup body
} // DEFER: cleanup end

// direct's defer is open-coded: it returns by calling cleanup itself.
//
//go:noinline
func direct() int {
	defer cleanup(1)
	sink++      // DEFER: direct body
	return sink // DEFER: direct return
} // DEFER: direct end

// looped's defers are records the runtime calls from deferreturn.
//
//go:noinline
func looped(count int) int {
	for note := range count {
		defer cleanup(note)
	}
	return sink // DEFER: looped return
} // DEFER: looped end

//go:noinline
func rescue() (caught bool) {
	defer func() { // DEFER: rescuer
		caught = recover() != nil // DEFER: recover
	}() // DEFER: rescuer end
	explode() // DEFER: explode call
	return false
}

//go:noinline
func explode() {
	panic("boom") // DEFER: panic
}

func main() {
	direct()  // DEFER: direct call
	looped(2) // DEFER: looped call
	rescue()  // DEFER: rescue call
	sink++    // DEFER: done
}
