package app

import "example.com/gtwin/lib"

func Use() string {
	b := lib.New()
	return b.Bind()
}
