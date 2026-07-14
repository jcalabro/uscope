package main

import "runtime"

type inner struct {
	signedValue   int32
	unsignedValue uint32
}

type outer struct {
	inner
	values [2]int32
}

var globalRecord = outer{inner: inner{signedValue: -7, unsignedValue: 9}, values: [2]int32{20, 22}}

//go:noinline
func inspectRecords(record *outer, records *[2]outer, slice []outer) bool {
	runtime.KeepAlive(record)
	runtime.KeepAlive(records)
	runtime.KeepAlive(slice)
	return record.signedValue == -7 && record.unsignedValue == 9 &&
		records[1].values[1] == 44 && slice[0].values[0] == 20
}

func main() {
	records := [2]outer{
		globalRecord,
		{inner: inner{signedValue: 5, unsignedValue: 6}, values: [2]int32{43, 44}},
	}
	if !inspectRecords(&globalRecord, &records, records[:]) {
		panic("record fixture failed")
	}
}
