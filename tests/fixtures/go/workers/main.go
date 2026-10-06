// A pool of workers, and goroutines parked in every state a program names,
// checked against the runtime's own account of them.
//
// Before each checkpoint every other goroutine is parked where the program
// put it, which the program observes in its own goroutine dump rather than
// waiting for. The checkpoint then prints `TRUTH` lines from the runtime:
// each goroutine's id, status, and frames, and the thread running main.
// `runtime.Stack` dumps as `GOTRACEBACK=single` does whatever the setting,
// so the dump leaves out the runtime's own goroutines and frames.
package main

import (
	"fmt"
	"os"
	"runtime"
	"strings"
	"sync"
	"syscall"
	"time"
)

// sink keeps calls the compiler would otherwise drop.
var sink int

// reached is where tests stop at a checkpoint.
//
//go:noinline
func reached(name string) {
	sink += len(name)
}

// truth prints one tab-separated line a test checks the debugger against.
func truth(fields ...any) {
	text := make([]string, len(fields))
	for index, field := range fields {
		text[index] = fmt.Sprint(field)
	}
	fmt.Println("TRUTH\t" + strings.Join(text, "\t"))
}

// goroutine is one goroutine of a dump: its id, status, and frames.
type goroutine struct {
	id     string
	status string
	frames []string
}

// dump parses the runtime's dump of every goroutine the program started.
func dump() []goroutine {
	buffer := make([]byte, 1<<20)
	text := string(buffer[:runtime.Stack(buffer, true)])
	var goroutines []goroutine
	for _, block := range strings.Split(strings.TrimSpace(text), "\n\n") {
		lines := strings.Split(block, "\n")
		// goroutine 18 [chan receive, 2 minutes, locked to thread]:
		header := strings.TrimSuffix(strings.TrimPrefix(lines[0], "goroutine "), ":")
		id, rest, _ := strings.Cut(header, " ")
		status := strings.TrimSuffix(strings.TrimPrefix(rest, "["), "]")
		status, _, _ = strings.Cut(status, ",")
		current := goroutine{id: id, status: status}
		// Each frame is a call line followed by its tab-indented position;
		// the dump may end with the line that created the goroutine.
		for index := 1; index+1 < len(lines); index += 2 {
			call := lines[index]
			if strings.HasPrefix(call, "created by ") {
				break
			}
			function := call[:strings.LastIndex(call, "(")]
			position := strings.TrimSpace(lines[index+1])
			position, _, _ = strings.Cut(position, " ")
			current.frames = append(current.frames, function+"\t"+position)
		}
		goroutines = append(goroutines, current)
	}
	return goroutines
}

// me is the calling goroutine's id, from the header of its own dump.
func me() string {
	buffer := make([]byte, 64)
	header := string(buffer[:runtime.Stack(buffer, false)])
	id, _, _ := strings.Cut(strings.TrimPrefix(header, "goroutine "), " ")
	return id
}

// awaitStatuses yields until the dump shows a goroutine in each status.
func awaitStatuses(statuses ...string) {
	for {
		seen := map[string]int{}
		for _, goroutine := range dump() {
			seen[goroutine.status]++
		}
		missing := false
		for _, status := range statuses {
			if seen[status] == 0 {
				missing = true
			}
			seen[status]--
		}
		if !missing {
			return
		}
		runtime.Gosched()
	}
}

// checkpoint prints the runtime's account of every goroutine, then stops.
func checkpoint(name string) {
	goroutines := dump()
	truth("checkpoint", name)
	truth("count", runtime.NumGoroutine())
	truth("main", me(), syscall.Gettid())
	for _, goroutine := range goroutines {
		truth("task", goroutine.id, goroutine.status)
		for _, frame := range goroutine.frames {
			truth("frame", goroutine.id, frame)
		}
	}
	reached(name)
}

// worker squares the jobs it receives until there are none left.
func worker(jobs <-chan int, results chan<- int, group *sync.WaitGroup) {
	defer group.Done()
	for job := range jobs {
		results <- job * job
	}
}

func main() {
	// The thread main reports stays the one it runs on.
	runtime.LockOSThread()

	jobs := make(chan int)
	results := make(chan int, 16)
	var workers sync.WaitGroup
	for range 4 {
		workers.Add(1)
		go worker(jobs, results, &workers)
	}

	// One goroutine for each other way a program parks one.
	var lock sync.Mutex
	lock.Lock()
	go func() {
		lock.Lock()
		lock.Unlock()
	}()

	never := make(chan int)
	done := make(chan struct{})
	go func() {
		select {
		case <-never:
		case <-done:
		}
	}()

	go func() {
		time.Sleep(time.Hour)
	}()

	var waiting sync.Mutex
	condition := sync.NewCond(&waiting)
	signalled := false
	go func() {
		waiting.Lock()
		for !signalled {
			condition.Wait()
		}
		waiting.Unlock()
	}()

	reader, writer, err := os.Pipe()
	if err != nil {
		panic(err)
	}
	go func() {
		buffer := make([]byte, 1)
		reader.Read(buffer)
	}()

	// A blocking read the runtime does not poll keeps its thread in the
	// system call.
	var raw [2]int
	if err := syscall.Pipe(raw[:]); err != nil {
		panic(err)
	}
	go func() {
		buffer := make([]byte, 1)
		syscall.Read(raw[0], buffer)
	}()

	awaitStatuses(
		"chan receive", "chan receive", "chan receive", "chan receive",
		"sync.Mutex.Lock", "select", "sleep", "sync.Cond.Wait", "IO wait",
		"syscall",
	)
	checkpoint("parked")

	for job := range 8 {
		jobs <- job
	}
	close(jobs)
	workers.Wait()
	close(results)
	total := 0
	for result := range results {
		total += result
	}

	lock.Unlock()
	close(done)
	waiting.Lock()
	signalled = true
	condition.Broadcast()
	waiting.Unlock()
	writer.Write([]byte{1})
	syscall.Write(raw[1], []byte{1})

	if total != 140 {
		os.Exit(1)
	}
}
