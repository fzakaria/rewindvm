// Prints from Greeter.Greet, first on a goroutine of its own, with a slice
// for the Go runtime's gdb support.
package main

import (
	"fmt"
	"sync"
)

type Greeter struct {
	Name   string
	Counts []int
}

//go:noinline
func (g *Greeter) Greet(who string) {
	g.Counts = append(g.Counts, len(who))
	fmt.Println("hello from go,", who, g.Name, g.Counts)
}

func main() {
	g := &Greeter{Name: "go", Counts: []int{1, 2, 3}}
	var wg sync.WaitGroup
	wg.Add(1)
	go func() { defer wg.Done(); g.Greet("alice") }()
	wg.Wait()
	g.Greet("bob")
}
