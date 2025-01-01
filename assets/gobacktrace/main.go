package main

import "fmt"

func FuncE() {
	fmt.Println("FuncE")
}

func FuncD() {
	FuncE()
	fmt.Println("FuncD")
}

func FuncC() {
	FuncD()
	fmt.Println("FuncC")
}

func FuncB() {
	FuncC()
	fmt.Println("FuncB")
}

func FuncA() {
	FuncB()
	fmt.Println("FuncA")
}

func FuncF() {
	fmt.Println("FuncF")
	FuncE()
}

func main() {
	FuncA()
	FuncB()
	FuncC()
	FuncD()
	FuncE()
	FuncF()
}
