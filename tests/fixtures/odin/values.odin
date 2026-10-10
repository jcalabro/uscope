// Odin's values, where this program says they are. Before each checkpoint
// the program prints its own truth, one tab-separated line per value,
//
//	TRUTH	<checkpoint>	<path>	<kind>	<value>
//
// and then calls reached(checkpoint); the tests read the values in
// reached's caller. A path names a variable and then its members and
// elements. Floats are their bits in hexadecimal; kind `summary` is how
// uscope writes the value.
package values

import "base:intrinsics"
import "core:fmt"

Point :: struct {
	x: i32,
	y: i32,
}

Segment :: struct {
	from: Point,
	to:   Point,
	tag:  u8,
}

Color :: enum u8 {
	Red,
	Green,
	Blue,
}

Shape :: union {
	int,
	f64,
	string,
}

sink: rawptr

// Reaches a checkpoint; fmt.printf has written each truth before it returns.
reached :: #force_no_inline proc(checkpoint: string) {
	intrinsics.volatile_store(&sink, raw_data(checkpoint))
}

truth :: proc(checkpoint, path, kind: string, value: any) {
	fmt.printf("TRUTH\t%s\t%s\t%s\t%v\n", checkpoint, path, kind, value)
}

text_truth :: proc(checkpoint, path, value: string) {
	fmt.printf("TRUTH\t%s\t%s\tstring\t\"%s\"\n", checkpoint, path, value)
}

f32_truth :: proc(checkpoint, path: string, value: f32) {
	fmt.printf("TRUTH\t%s\t%s\tf32\t%#x\n", checkpoint, path, transmute(u32)value)
}

f64_truth :: proc(checkpoint, path: string, value: f64) {
	fmt.printf("TRUTH\t%s\t%s\tf64\t%#x\n", checkpoint, path, transmute(u64)value)
}

// Keeps a value alive, and where the program put it, past the checkpoint.
keep :: #force_no_inline proc(value: rawptr) {
	intrinsics.volatile_store(&sink, value)
}

add :: #force_no_inline proc(a, b: int) -> int {
	sum := a + b
	keep(&sum)
	return sum
}

scalars :: #force_no_inline proc() {
	small: i8 = -5
	wide: u16 = 65000
	big: i64 = -1 << 40
	single: f32 = 1.5
	double: f64 = -0.1
	flag := true
	letter: rune = 'λ'
	truth("scalars", "small", "int", small)
	truth("scalars", "wide", "int", wide)
	truth("scalars", "big", "int", big)
	f32_truth("scalars", "single", single)
	f64_truth("scalars", "double", double)
	truth("scalars", "flag", "summary", flag)
	truth("scalars", "letter", "int", i32(letter))
	reached("scalars")
	keep(&small); keep(&wide); keep(&big); keep(&single); keep(&double); keep(&flag); keep(&letter)
}

records :: #force_no_inline proc() {
	point := Point{3, -4}
	segment := Segment{Point{1, 2}, Point{5, 6}, 9}
	numbers := [3]i32{10, 20, 30}
	grid := [2][2]u8{{1, 2}, {3, 4}}
	color := Color.Green
	truth("records", "point.x", "int", point.x)
	truth("records", "point.y", "int", point.y)
	truth("records", "segment.to.y", "int", segment.to.y)
	truth("records", "segment.tag", "int", segment.tag)
	for number, index in numbers {
		truth("records", fmt.tprintf("numbers.%d", index), "int", number)
	}
	truth("records", "grid.1.0", "int", grid[1][0])
	truth("records", "color", "symbol", color)
	reached("records")
	keep(&point); keep(&segment); keep(&numbers); keep(&grid); keep(&color)
}

slices :: #force_no_inline proc() {
	backing := [4]i64{7, 8, 9, 10}
	ints := backing[1:]
	nothing: []i64
	text := "héllo"
	empty := ""
	growing := make([dynamic]u32, 0, 8)
	defer delete(growing)
	append(&growing, 100, 200)
	ctext: cstring = "terminated"
	truth("slices", "ints", "len", len(ints))
	for value, index in ints {
		truth("slices", fmt.tprintf("ints.%d", index), "int", value)
	}
	truth("slices", "nothing", "len", len(nothing))
	text_truth("slices", "text", text)
	text_truth("slices", "empty", empty)
	truth("slices", "growing", "len", len(growing))
	truth("slices", "growing.1", "int", growing[1])
	text_truth("slices", "ctext", string(ctext))
	reached("slices")
	keep(&ints); keep(&nothing); keep(&text); keep(&empty); keep(&growing); keep(&ctext)
}

unions :: #force_no_inline proc() {
	whole: Shape = 7
	real: Shape = 2.5
	named: Shape = "circle"
	none: Shape
	some: Maybe(int) = 5
	missing: Maybe(int)
	truth("unions", "whole.v1", "int", whole.(int))
	f64_truth("unions", "real.v2", real.(f64))
	text_truth("unions", "named.v3", named.(string))
	truth("unions", "none", "summary", "nil")
	truth("unions", "some.v1", "int", some.?)
	truth("unions", "missing", "summary", "nil")
	reached("unions")
	keep(&whole); keep(&real); keep(&named); keep(&none); keep(&some); keep(&missing)
}

main :: proc() {
	scalars()
	records()
	slices()
	unions()
	total := add(2, 3)
	if total != 5 {
		panic("add")
	}
}
