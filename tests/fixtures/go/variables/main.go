package main

import "runtime"

var goGlobal int64 = 73
var goSink int64

type scalarAlias = int32
type definedInt int32

type pointerPair struct {
	first  int32
	second int32
}

type pointerNode struct {
	next  *pointerNode
	value int32
}

type recursiveList[T any] struct {
	next  *recursiveList[T]
	value T
}

//go:noinline
func inspectScalars(
	flag bool,
	signedValue int32,
	unsignedValue uint64,
	single float32,
	doublePrecision float64,
	pointerParameter *int32,
	pointerPointer **int32,
	nilPointer *int32,
	structurePointer *pointerPair,
	recursivePointer *pointerNode,
	sliceValue []int32,
) bool {
	localFlag := !flag
	localSigned := signedValue + 1
	localUnsigned := unsignedValue + 2
	localSingle := single + 0.5
	localDouble := doublePrecision - 0.25
	localAlias := scalarAlias(signedValue)
	localDefined := definedInt(signedValue)
	localList := recursiveList[int32]{value: signedValue}
	localList.next = &localList
	goSink = int64(localSigned)
	optimizedAway := signedValue * 3
	_ = optimizedAway
	runtime.KeepAlive(pointerParameter)
	runtime.KeepAlive(pointerPointer)
	runtime.KeepAlive(nilPointer)
	runtime.KeepAlive(structurePointer)
	runtime.KeepAlive(recursivePointer)
	runtime.KeepAlive(sliceValue)
	runtime.KeepAlive(localFlag)
	runtime.KeepAlive(localUnsigned)
	runtime.KeepAlive(localSingle)
	runtime.KeepAlive(localDouble)
	runtime.KeepAlive(localAlias)
	runtime.KeepAlive(localDefined)
	runtime.KeepAlive(localList)
	return !localFlag && localSigned == -41 && *pointerParameter == 42 &&
		**pointerPointer == 42 && structurePointer.first+structurePointer.second == 42 &&
		recursivePointer.next == nil && recursivePointer.value == 42 &&
		len(sliceValue) == 2 && localUnsigned == 44 &&
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

//go:noinline
func inspectSlices(values []int32, empty []int32, nilSlice []int32) bool {
	runtime.KeepAlive(values)
	runtime.KeepAlive(empty)
	runtime.KeepAlive(nilSlice)
	return len(values) == 2 && cap(values) == 3 && len(empty) == 0 && cap(empty) == 4 && nilSlice == nil
}

func main() {
	pointerValue := int32(42)
	pointer := &pointerValue
	pair := pointerPair{first: 20, second: 22}
	node := pointerNode{value: 42}
	slice := []int32{20, 22}
	backing := []int32{10, 20, 22, 40}
	succeeded := inspectScalars(
		true, -42, 42, 1.25, -2.5,
		pointer, &pointer, nil, &pair, &node, slice,
	)
	succeeded = succeeded && inspectSlices(backing[1:3], backing[:0], nil)
	inspectShadow()
	if !succeeded {
		goSink = goGlobal
	}
}
