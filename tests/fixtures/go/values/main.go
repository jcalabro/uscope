// A gallery of values as Go shows them. Before each checkpoint the program
// prints its own truth, one tab-separated line per value,
//
//	TRUTH	<checkpoint>	<path>	<kind>	<value>
//
// and then calls reached(checkpoint); the tests inspect reached's caller.
// A path names a variable, or a child of one after a dot. Floats are their
// bits in hexadecimal; complex numbers are their parts' bits, real first.
// Kind `summary` is how uscope writes the value.
package main

import (
	"fmt"
	"math"
	"os"
	"runtime"
)

var sink any

//go:noinline
func reached(checkpoint string) {
	sink = checkpoint
}

func truth(checkpoint, path, kind string, value any) {
	fmt.Printf("TRUTH\t%s\t%s\t%s\t%v\n", checkpoint, path, kind, value)
}

func bits32(value float32) string { return fmt.Sprintf("%#x", math.Float32bits(value)) }
func bits64(value float64) string { return fmt.Sprintf("%#x", math.Float64bits(value)) }

//go:noinline
func complexes(small complex64, large complex128) complex128 {
	product := large * complex(2, 0)
	truth("complex", "small", "c64", bits32(real(small))+":"+bits32(imag(small)))
	truth("complex", "large", "c128", bits64(real(large))+":"+bits64(imag(large)))
	truth("complex", "product", "c128", bits64(real(product))+":"+bits64(imag(product)))
	truth("complex", "small.imag", "f32", bits32(imag(small)))
	truth("complex", "large.real", "f64", bits64(real(large)))
	truth("complex", "small", "summary", "(1.5-2i)")
	truth("complex", "large", "summary", "(0.1+3e300i)")
	reached("complex")
	runtime.KeepAlive(small)
	runtime.KeepAlive(large)
	return product
}

// Point is passed in two registers.
type Point struct{ X, Y int }

// pieces takes values Go passes in registers, which its code describes in
// pieces; the tests stop at its entry rather than in reached's caller.
//
//go:noinline
func pieces(text string, numbers []int, boxed any, pair Point, ratio complex128) int {
	return len(text) + len(numbers) + pair.X + len(fmt.Sprint(boxed)) + int(real(ratio))
}

func main() {
	if complexes(complex(1.5, -2), complex(0.1, 3e300)) == 0 {
		os.Exit(1)
	}
	text := "pieces"
	numbers := []int{4, 5, 6}
	truth("pieces", "text", "string", fmt.Sprintf("%q", text))
	truth("pieces", "numbers", "len", len(numbers))
	truth("pieces", "numbers.1", "int", numbers[1])
	truth("pieces", "pair.X", "int", 7)
	truth("pieces", "pair.Y", "int", 8)
	truth("pieces", "ratio", "c128", bits64(0.5)+":"+bits64(-1))
	pieces(text, numbers, 42, Point{X: 7, Y: 8}, complex(0.5, -1))
}
