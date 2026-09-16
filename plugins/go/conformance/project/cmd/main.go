package main

import (
	"fmt"

	"github.com/example/app/server"
)

func main() {
	s := server.New()
	fmt.Println(s.Addr())
}
