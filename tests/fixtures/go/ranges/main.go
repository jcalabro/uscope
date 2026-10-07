// Loops over iterator functions. Each loop's body is a function of its
// own that the iterator calls, which a step in the loop treats as the
// loop's own code: it goes from one pass of the body to the next, and out
// after the loop, never into the iterator. Count is never inlined, so its
// body is a function of its own when optimized too; Evens is small
// enough to be inlined, with the body inside it, when optimized.
package main

import (
	"fmt"
	"iter"
)

// Count yields 0 through n-1.
//
//go:noinline
func Count(n int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for i := 0; i < n; i++ {
			if !yield(i) {
				return
			}
		}
	}
}

// Evens yields the even numbers below n.
func Evens(n int) iter.Seq[int] {
	return func(yield func(int) bool) {
		for i := 0; i < n; i += 2 {
			if !yield(i) {
				return
			}
		}
	}
}

// sink keeps a result the compiler would otherwise drop.
var sink int

//go:noinline
func counted() int {
	total := 0
	scale := 3
	for v := range Count(5) { // WALK: counted loop
		total += v * scale // WALK: counted add
		if v == 3 {        // WALK: counted check
			break // WALK: counted break
		}
	} // WALK: counted end
	return total // WALK: counted after
}

//go:noinline
func evens() int {
	total := 0
	for v := range Evens(6) { // WALK: evens loop
		total += v // WALK: evens add
	} // WALK: evens end
	sink = total // WALK: evens after
	return total
}

func main() {
	fmt.Println(counted(), evens())
}
