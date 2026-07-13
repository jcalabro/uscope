package main

import "runtime"

var goGlobal int64 = 73
var goSink int64

//go:noinline
func inspectScalars(
	flag bool,
	signedValue int32,
	unsignedValue uint64,
	single float32,
	doublePrecision float64,
) bool {
	localFlag := !flag
	localSigned := signedValue + 1
	localUnsigned := unsignedValue + 2
	localSingle := single + 0.5
	localDouble := doublePrecision - 0.25
	goSink = int64(localSigned)
	optimizedAway := signedValue * 3
	_ = optimizedAway
	runtime.KeepAlive(localFlag)
	runtime.KeepAlive(localUnsigned)
	runtime.KeepAlive(localSingle)
	runtime.KeepAlive(localDouble)
	return !localFlag && localSigned == -41 && localUnsigned == 44 &&
		localSingle == 1.75 && localDouble == -2.75
}

//go:noinline
func inspectShadow() {
	shadowed := int32(100)
	{
		shadowed := int32(200)
		goSink = int64(shadowed)
		runtime.KeepAlive(shadowed)
	}
	runtime.KeepAlive(shadowed)
}

func main() {
	succeeded := inspectScalars(true, -42, 42, 1.25, -2.5)
	inspectShadow()
	if !succeeded {
		goSink = goGlobal
	}
}
