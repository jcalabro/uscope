// A simple program that prints a bunch of basic data types
package main

import "log"

type BasicStruct struct {
	A int
	b string
	c NestedStruct
}

type NestedStruct struct {
	D int
	E int
}

func main() {
	a := uint8(1)
	b := uint16(2)
	c := uint32(3)
	d := uint64(4)

	e := int8(5)
	f := int16(6)
	g := int32(7)
	h := int64(8)

	i := float32(8)
	j := float64(8)

	k := 9

	l := true
	m := false

	n := "hello!"

	o := []int{1, 2, 3}
	p := []string{"hi", "hey", "hello there"}

	q := make(chan string, 10)
	q <- "this is the channel message"

	r := BasicStruct{
		A: 123,
		b: "basic struct",
		c: NestedStruct{
			D: 456,
			E: 789,
		},
	}

	log.Printf("a: %v", a)
	log.Printf("b: %v", b)
	log.Printf("c: %v", c)
	log.Printf("d: %v", d)

	log.Printf("e: %v", e)
	log.Printf("f: %v", f)
	log.Printf("g: %v", g)
	log.Printf("h: %v", h)

	log.Printf("i: %v", i)
	log.Printf("j: %v", j)

	log.Printf("k: %v", k)

	log.Printf("l: %v", l)
	log.Printf("m: %v", m)

	log.Printf("n: %v", n)

	log.Printf("o: %v", o)
	log.Printf("p: %v", p)

	log.Printf("q: %v", q)

	log.Printf("r: %v", r)
}
