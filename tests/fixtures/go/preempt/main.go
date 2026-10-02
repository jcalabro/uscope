// Goroutines that compute long enough for the Go runtime to preempt them
// asynchronously, which it does by sending the thread SIGURG.
package main

import (
	"os"
	"sync"
)

//go:noinline
func spin(count int) int {
	total := 0
	for index := 0; index < count; index++ {
		total += index % 7
	}
	return total
}

func main() {
	var group sync.WaitGroup
	results := make([]int, 4)
	for worker := range results {
		group.Add(1)
		go func() {
			defer group.Done()
			results[worker] = spin(200_000_000)
		}()
	}
	group.Wait()
	for _, result := range results {
		if result != results[0] {
			os.Exit(1)
		}
	}
}
