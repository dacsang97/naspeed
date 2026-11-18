package greet

import (
	_ "embed"
	"strings"
)

//go:embed greeting*.txt
var greeting string

func Text() string { return strings.TrimSpace(greeting) }
