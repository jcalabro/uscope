// Runs a child process. The Go runtime starts it with a vfork-style clone,
// so the child runs the rest of syscall.forkAndExecInChild1, including
// runtime_AfterForkInChild, in this program's memory until it execs. The
// program exits 3 if the child did not exec cleanly.
package main

import (
	"os"
	"os/exec"
)

func main() {
	if err := exec.Command("true").Run(); err != nil {
		os.Exit(3)
	}
	os.Exit(0)
}
