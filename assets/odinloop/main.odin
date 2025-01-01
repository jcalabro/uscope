package main

import "core:fmt"
import "core:sys/linux"
import "core:time"

iterate :: proc(pid: linux.Pid, ndx: ^int) {
	fmt.printf("odin looping (pid %d): %d\n", pid, ndx^)
	ndx^ += 1
	time.sleep(time.Second)
}

main :: proc() {
	pid := linux.getpid()
	ndx := 0
	for {
		iterate(pid, &ndx)
	}
}
