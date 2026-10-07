// A server that runs on its own, for a debugger to attach to. It prints
// READY and its address once it accepts connections, answers each
// /count with how many it has answered, and ends when asked to /quit.
package main

import (
	"context"
	"fmt"
	"net"
	"net/http"
	"os"
	"sync"
	"sync/atomic"
)

var counted atomic.Int64

// count answers with how many counts have been asked for.
func count(writer http.ResponseWriter, request *http.Request) {
	total := counted.Add(1)
	fmt.Fprint(writer, total) // SERVED: count
}

func main() {
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	quit := make(chan struct{})
	var once sync.Once
	mux := http.NewServeMux()
	mux.HandleFunc("/count", count)
	mux.HandleFunc("/quit", func(writer http.ResponseWriter, request *http.Request) {
		fmt.Fprint(writer, "bye")
		once.Do(func() { close(quit) })
	})
	server := &http.Server{Handler: mux}
	go server.Serve(listener)
	fmt.Printf("READY %s\n", listener.Addr())
	<-quit
	server.Shutdown(context.Background())
}
