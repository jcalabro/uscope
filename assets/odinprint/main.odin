package main

import "core:fmt"

MyStruct :: struct {
	first: int,
	second: string,
}

main :: proc() {
	// booleans
	a: bool = true
	b: b8 = false
	c: b16 = true
	d: b32 = false
	e: b64 = true

	// integers
	f: int = 0
	g: i8 = 1
	h: i16 = 2
	i: i32 = 3
	j: i64 = 4
	k: uint = 5
	l: u8 = 6
	m: u16 = 7
	n: u32 = 8
	o: u64 = 9

	// floats
	p: f16 = 10.11
	q: f32 = 11.12
	r: f64 = 12.13

	// @TODO (jrc): endian specific integers
	// @TODO (jrc): endian specific floating point numbers
	// @TODO (jrc): complex numbers
	// @TODO (jrc): quaternion numbers

	// strings
	s: string = "this is a sample string"
	t: cstring = "this is a cstring"
	u := &s
	v := &t

	// arrays
	w := []int{14, 15, 16}
	x := [dynamic]int{}
	append(&x, 17)
	append(&x, 18)

	// @TODO (jrc): maps

	// structs
	y := MyStruct{
		13,
		"this is the second field",
	}
	z := &y

	aa := uintptr(k)
	ab := rawptr(aa)

	MyEnum :: enum {First, Second, Third}
	ac := MyEnum.First
	ad := MyEnum.Second
	ae := MyEnum.Third

	fmt.println(a)
	fmt.println(b)
	fmt.println(c) // sim:odinprint stops here
	fmt.println(d)
	fmt.println(e)

	fmt.println(f)
	fmt.println(g)
	fmt.println(h)
	fmt.println(i)
	fmt.println(j)
	fmt.println(k)
	fmt.println(l)
	fmt.println(m)
	fmt.println(n)
	fmt.println(o)

	fmt.println(p)
	fmt.println(q)
	fmt.println(r)

	fmt.println(s)
	fmt.println(t)
	fmt.println(u)
	fmt.println(v)

	fmt.println(w)
	fmt.println(x)

	fmt.println(y)
	fmt.println(z)

	fmt.println(aa)
	fmt.println(ab)

	fmt.println(ac)
	fmt.println(ad)
	fmt.println(ae)
}
