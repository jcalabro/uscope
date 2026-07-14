package main

import "runtime"

var packageValue int64 = 161
var packageMutable int32 = 162
var packagePointer = &packageMutable
var packagePointerPointer = &packagePointer
var packageNil *int32
var packagePair = struct {
	first  int32
	second int32
}{20, 22}
var packagePairPointer = &packagePair
var globalSink int64
var optimizedAway int64 = 169

//go:noinline
func inspectGlobals() {
	globalSink = packageValue + int64(packageMutable)
	runtime.KeepAlive(globalSink)
	runtime.KeepAlive(packagePointerPointer)
	runtime.KeepAlive(packageNil)
	runtime.KeepAlive(packagePairPointer)
}

func main() {
	inspectGlobals()
	if globalSink != 323 {
		globalSink = optimizedAway
	}
}
