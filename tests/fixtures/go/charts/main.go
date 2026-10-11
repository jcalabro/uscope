// A web server's counters, drawn by the built-in renderers through
// charts.views beside this file: requests by path and bytes by content
// type, both maps whose order Go randomizes, and requests by hour.
package main

import (
	"fmt"
	"os"
)

// Hits counts requests by path.
type Hits map[string]int

// Bytes totals the bytes served by content type.
type Bytes map[string]int64

// Hours counts requests by hour of the day.
type Hours [24]int

func record(hits Hits, bytes Bytes, hours *Hours, tick int) {
	paths := []string{"/api/search", "/api/items", "/static/app.js", "/api/login", "/api/cart", "/healthz"}
	for i, path := range paths {
		hits[path] += (len(paths) - i) * 100
	}
	types := []string{"text/html", "application/json", "image/png", "text/css", "image/svg+xml", "font/woff2", "text/plain", "application/wasm", "image/webp", "text/csv"}
	for i, kind := range types {
		bytes[kind] += int64((len(types)-i)*(len(types)-i)) * 1000
	}
	for hour := range hours {
		hours[hour] += 10 + (hour*7+tick)%13
	}
}

func main() {
	hits := Hits{}
	bytes := Bytes{}
	var hours Hours
	// Fifty paths more than a chart shows, each requested once.
	for i := range 50 {
		hits[fmt.Sprintf("/static/asset-%02d.png", i)] = 1
	}
	for tick := 1; tick <= 100; tick++ {
		record(hits, bytes, &hours, tick)
		fmt.Fprintf(os.Stdout, "tick %d: %d paths\n", tick, len(hits))
	}
}
