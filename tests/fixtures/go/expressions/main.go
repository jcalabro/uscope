// Prints `EXPECT\t<expression>\t<kind>\t<value>` for each expression the
// debugger must agree with, then calls barrier().
package main

import (
	"fmt"
	"math"
	"runtime"
)

type inner struct {
	s  int16
	ll int64
}

type fixture struct {
	small int8
	byte_ uint8
	count int
	inner inner
	arr   [4]int32
	slice []int32
	text  string
	ptr   *int32
	flag  bool
	real  float64
}

//go:noinline
func barrier(f *fixture) {
	runtime.KeepAlive(f)
}

func main() {
	f := &fixture{
		small: -100,
		byte_: 250,
		count: -70000,
		inner: inner{s: -12, ll: 123456789012},
		arr:   [4]int32{10, 20, 30, 40},
		slice: []int32{7, 8, 9},
		text:  "hello",
		flag:  true,
		real:  2.75,
	}
	f.ptr = &f.arr[2]
	fmt.Printf("EXPECT\tf.small\tint\t%d\n", f.small)
	fmt.Printf("EXPECT\tf.byte_ + 10\tint\t%d\n", int(f.byte_)+10)
	fmt.Printf("EXPECT\tf.count * 2\tint\t%d\n", f.count*2)
	fmt.Printf("EXPECT\tf.inner.ll\tint\t%d\n", f.inner.ll)
	fmt.Printf("EXPECT\tf.arr[3]\tint\t%d\n", f.arr[3])
	fmt.Printf("EXPECT\tf.slice[1]\tint\t%d\n", f.slice[1])
	fmt.Printf("EXPECT\tlen(f.slice)\tint\t%d\n", len(f.slice))
	fmt.Printf("EXPECT\tf.text == \"hello\"\tbool\t%t\n", f.text == "hello")
	fmt.Printf("EXPECT\tlen(f.text)\tint\t%d\n", len(f.text))
	fmt.Printf("EXPECT\t*f.ptr\tint\t%d\n", *f.ptr)
	fmt.Printf("EXPECT\tf.flag\tbool\t%t\n", f.flag)
	fmt.Printf("EXPECT\tf.real * 2\tf64\t%#x\n", math.Float64bits(f.real*2))
	barrier(f)
	runtime.KeepAlive(f)
}
