// A gallery of values as Go shows them. Before each checkpoint the program
// prints its own truth, one tab-separated line per value,
//
//	TRUTH	<checkpoint>	<path>	<kind>	<value>
//
// and then calls reached(checkpoint); the tests inspect reached's caller.
// A path names a variable, or a child of one after a dot. Floats are their
// bits in hexadecimal.
package main

import (
	"fmt"
)

var sink any

//go:noinline
func reached(checkpoint string) {
	sink = checkpoint
}

func truth(checkpoint, path, kind string, value any) {
	fmt.Printf("TRUTH\t%s\t%s\t%s\t%v\n", checkpoint, path, kind, value)
}

// Point is passed in two registers.
type Point struct{ X, Y int }

// pieces takes values Go passes in registers, which its code describes in
// pieces; the tests stop at its entry rather than in reached's caller.
//
//go:noinline
func pieces(text string, numbers []int, boxed any, pair Point) int {
	return len(text) + len(numbers) + pair.X + len(fmt.Sprint(boxed))
}

func main() {
	text := "pieces"
	numbers := []int{4, 5, 6}
	truth("pieces", "text", "string", fmt.Sprintf("%q", text))
	truth("pieces", "numbers", "len", len(numbers))
	truth("pieces", "numbers.1", "int", numbers[1])
	truth("pieces", "pair.X", "int", 7)
	truth("pieces", "pair.Y", "int", 8)
	pieces(text, numbers, 42, Point{X: 7, Y: 8})
}
