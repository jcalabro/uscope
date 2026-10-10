// Records the frames of a call chain as the runtime itself sees them, so a
// debugger's backtrace can be checked against them, and then stops in
// reached. The chain has a method, recursion, an inlined function, and an
// inlined closure.
package main

import (
	"fmt"
	"runtime"
)

var sink int

// Keeps the main goroutine on the main thread. A stripped program's steps
// follow their thread, and the runtime may otherwise resume a goroutine it
// preempted on another.
func init() {
	runtime.LockOSThread()
}

//go:noinline
func reached(name string) {
	sink += len(name)
}

// checkpoint prints the frames of its callers, inline frames included, as
// tab-separated TRUTH lines, then calls reached.
//
//go:noinline
func checkpoint(name string) {
	pcs := make([]uintptr, 64)
	// Skips runtime.Callers and checkpoint itself.
	frames := runtime.CallersFrames(pcs[:runtime.Callers(2, pcs)])
	for {
		frame, more := frames.Next()
		fmt.Printf("TRUTH\tframe\t%s\t%s\t%d\n", frame.Function, frame.File, frame.Line)
		if !more {
			break
		}
	}
	reached(name)
}

type walker struct {
	depth int
}

// relay is small enough that the compiler inlines it into descend.
func relay(w *walker) {
	checkpoint("descend")
}

//go:noinline
func (w *walker) descend(n int) {
	if n == 0 {
		relay(w)
		return
	}
	w.depth++ // descend
	w.descend(n - 1)
	sink += w.depth
}

func main() {
	run := func() {
		(&walker{}).descend(2)
	}
	run()
	fmt.Println("done", sink)
}
