// loop is a simple program that prints its pid over and over again
// once per second
package main

import (
	"fmt"
	"os"
	"time"
)

func main() {
	pid := os.Getpid()
	ndx := 0
	for {
		fmt.Printf("go looping (pid: %d): %d\n", pid, ndx)
		ndx++

		time.Sleep(time.Second)
	}
}
