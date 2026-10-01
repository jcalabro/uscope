package main

var sink int

// leaf's arguments overwrite the registers its callers' arguments arrived in.
//
//go:noinline
func leaf(x, y int) int {
	sink = x + y
	return sink
}

//go:noinline
func pass(x, y int) int {
	return leaf(1000, 2000) + 1
}

// Go preserves no general registers across calls, so held's arguments stay
// in rax and rbx through its call, dead once it returns.
//
//go:noinline
func held(a, b int) int {
	return pass(a, b)
}

// seed is live across the call, so it is kept on the stack.
//
//go:noinline
func spilled(seed int) int {
	return held(seed+1, seed+2) + seed
}

func main() {
	println(spilled(10))
}
