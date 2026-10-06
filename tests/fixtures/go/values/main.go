// A gallery of values as Go shows them. Before each checkpoint the program
// prints its own truth, one tab-separated line per value,
//
//	TRUTH	<checkpoint>	<path>	<kind>	<value>
//
// and then calls reached(checkpoint); the tests inspect reached's caller.
// A path names a variable, or a child of one after a dot. Floats are their
// bits in hexadecimal; complex numbers are their parts' bits, real first.
// Kind `summary` is how uscope writes the value, `absent` says the
// variable must not be listed, and `addressable` that it is in memory.
package main

import (
	"fmt"
	"math"
	"os"
	"reflect"
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

func functionName(function any) string {
	value := reflect.ValueOf(function)
	if value.IsNil() {
		return "nil"
	}
	return runtime.FuncForPC(value.Pointer()).Name()
}

func double(value int) int { return 2 * value }

type adder struct{ base int }

func (a adder) add(value int) int { return a.base + value }

// funcs holds a nil func, a named function, a closure, which captures
// offset by value and total by reference, and a method value.
//
//go:noinline
func funcs(n int) int {
	var none func(int) int
	named := double
	offset := n * 3
	total := 0
	closure := func(value int) int {
		total += value
		return value + offset + n
	}
	method := adder{base: n}.add
	truth("funcs", "none", "func", functionName(none))
	truth("funcs", "named", "func", functionName(named))
	truth("funcs", "closure", "func", functionName(closure))
	truth("funcs", "closure.offset", "int", offset)
	truth("funcs", "closure.n", "int", n)
	truth("funcs", "closure.total", "int", total)
	truth("funcs", "method", "func", functionName(method))
	reached("funcs")
	runtime.KeepAlive(none)
	return named(1) + closure(2) + method(3) + total
}

// escapes moves counter to the heap, where Go's debug information
// describes it by a pointer named &counter.
//
//go:noinline
func escapes(start int) *int {
	counter := start
	pointer := &counter
	sink = pointer
	counter += 2
	truth("escape", "counter", "int", counter)
	truth("escape", "counter", "addressable", "")
	truth("escape", "&counter", "absent", "")
	reached("escape")
	return pointer
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
	if funcs(5) == 0 {
		os.Exit(1)
	}
	if *escapes(40) != 42 {
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
