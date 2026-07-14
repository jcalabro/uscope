package main

import "runtime"

var packageValue int64 = 161
var packageMutable int32 = 162
var globalSink int64
var optimizedAway int64 = 169

//go:noinline
func inspectGlobals() {
	globalSink = packageValue + int64(packageMutable)
	runtime.KeepAlive(globalSink)
}

func main() {
	inspectGlobals()
	if globalSink != 323 {
		globalSink = optimizedAway
	}
}
