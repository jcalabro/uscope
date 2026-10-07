// A map that grows while the program fills it. Its first table outgrows
// the largest a table may be and splits in two. Whenever the program is
// stopped, the map holds every key below `next`, each mapped to seven
// times itself, and no other.
package main

//go:noinline
func fill(entries map[int]int, count int) {
	for next := 0; next < count; next++ {
		entries[next] = 7 * next
	}
}

func main() {
	entries := map[int]int{}
	fill(entries, 1024)
}
