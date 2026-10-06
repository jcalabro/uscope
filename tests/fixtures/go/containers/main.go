// Maps, which the built-in views present. Each `VIEW:` marker says what its
// expression must show, evaluated in main() where barrier() is called;
// `(any order)` lets a map's entries come in any order, and `count:` gives
// only how many there are.
package main

import "runtime"

//go:noinline
func barrier(fixture any) {
	runtime.KeepAlive(fixture)
}

// Counts is a named map type, which views match as a map all the same.
type Counts map[string]int

func main() {
	small := map[string]int{"one": 1, "two": 2} // VIEW: small => len=2 {"one": 1, "two": 2} (any order)
	empty := map[int]int{}                       // VIEW: empty => len=0 {}
	var none map[int]int                         // VIEW: none => nil
	counts := Counts{"a": 1}                     // VIEW: counts => len=1 {"a": 1}
	big := map[int]int{}                         // VIEW: big => count: 300
	for index := 0; index < 300; index++ {
		big[index] = index * 10
	}
	// Large enough that its directory splits into several tables.
	huge := map[int]int{} // VIEW: huge => count: 5000
	for index := 0; index < 5000; index++ {
		huge[index] = index
	}
	barrier(&small)
	runtime.KeepAlive(empty)
	runtime.KeepAlive(none)
	runtime.KeepAlive(counts)
	runtime.KeepAlive(big)
	runtime.KeepAlive(huge)
}
