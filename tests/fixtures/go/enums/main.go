package main

import "runtime"

type State int32

const (
	StateNegative State = -3
	StateZero     State = 0
	StateAlias    State = 0
	StateReady    State = 7
)

//go:noinline
func inspectEnums(negative *State, alias *State, unknown *State) bool {
	runtime.KeepAlive(negative)
	runtime.KeepAlive(alias)
	runtime.KeepAlive(unknown)
	return *negative == StateNegative && *alias == StateAlias && *unknown == 5
}

func main() {
	negative := StateNegative
	alias := StateAlias
	unknown := State(5)
	if !inspectEnums(&negative, &alias, &unknown) {
		panic("unexpected enum fixture value")
	}
}
