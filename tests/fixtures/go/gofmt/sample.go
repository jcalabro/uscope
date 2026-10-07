// A file for gofmt to format, written as gofmt would not write it. It is
// gofmt's input, not a program.
package   sample

import "fmt"

type Point struct{ X,Y int }

var names = map[string]int{"a":1,"b":2}

func Show(p Point) string {
return fmt.Sprintf("%d,%d",p.X,p.Y)
}
