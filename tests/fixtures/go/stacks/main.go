// Goroutines whose calls into the runtime continue on a thread's system
// stack, each the way one of the runtime's stack switches makes it:
// `systemstack` runs a function there and returns, `morestack` grows a
// goroutine's stack and resumes it, and `mcall` parks a goroutine. A
// signal's handler runs on the thread's signal stack, and a fault becomes
// a call to `sigpanic`. Tests stop in the runtime and unwind back onto
// the goroutine's own stack.
package main

import (
	"fmt"
	"os"
	"os/signal"
	"runtime"
	"strings"
	"syscall"
	"unsafe"
)

// sink keeps calls the compiler would otherwise drop.
var sink int

// stats reads the memory statistics, which the runtime gathers on the
// system stack with the world stopped.
//
//go:noinline
func stats() {
	var memory runtime.MemStats
	runtime.ReadMemStats(&memory)
	sink += int(memory.NumGC)
}

// interrupt sends the program SIGUSR1 on the thread it runs on, so the
// runtime's handler runs above the call that sent it.
//
//go:noinline
func interrupt() {
	received := make(chan os.Signal, 1)
	signal.Notify(received, syscall.SIGUSR1)
	runtime.LockOSThread()
	syscall.Tgkill(syscall.Getpid(), syscall.Gettid(), syscall.SIGUSR1)
	runtime.UnlockOSThread()
	<-received
	signal.Stop(received)
}

// fault dereferences nil, which the runtime turns into a panic that it
// recovers.
//
//go:noinline
func fault() (recovered bool) {
	defer func() {
		recovered = recover() != nil
	}()
	var pointer *int
	sink += *pointer // the fault
	return false
}

// grow recurses deep enough that its goroutine's stack must grow.
//
//go:noinline
func grow(depth int) int {
	var pad [64]int
	pad[depth%64] = depth
	if depth == 0 {
		return pad[0]
	}
	return grow(depth-1) + pad[depth%64]
}

// below returns where its own local was, which is free once it returns.
//
//go:noinline
func below() uintptr {
	local := 7
	return uintptr(unsafe.Pointer(&local))
}

// hold reads the pointer stale holds.
//
//go:noinline
func hold(slot **int) {
	sink += int(uintptr(unsafe.Pointer(*slot)))
}

// stale holds a pointer below its own stack pointer, into memory its
// callees reuse, as a slot the runtime leaves unadjusted when it moves a
// stack does (go#75124).
//
//go:noinline
func stale() {
	pointer := (*int)(unsafe.Pointer(below()))
	hold(&pointer)
}

// awaitParked yields until the runtime's dump shows a goroutine that
// began in `function` waiting in `status`.
func awaitParked(function, status string) {
	buffer := make([]byte, 1<<16)
	for {
		dump := string(buffer[:runtime.Stack(buffer, true)])
		for _, block := range strings.Split(dump, "\n\n") {
			if strings.Contains(block, "["+status) && strings.Contains(block, function+"(") {
				return
			}
		}
		runtime.Gosched()
	}
}

func main() {
	stats()
	interrupt()
	if !fault() {
		os.Exit(1)
	}
	stale()
	// A goroutine that parks once, and stays parked.
	never := make(chan int)
	go func() {
		<-never
	}()
	done := make(chan int)
	go func() {
		done <- grow(2000)
	}()
	// Main parks until the result arrives.
	sink += <-done
	awaitParked("main.main.func1", "chan receive")
	fmt.Println(sink)
}
