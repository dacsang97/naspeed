package main

import (
	"flag"
	"fmt"
	"net/http"
	"os"

	"example.test/svc/internal/greet"
)

func main() {
	port := flag.Int("port", 19082, "listen port")
	flag.Parse()
	http.HandleFunc("/pid", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprint(w, os.Getpid())
	})
	http.HandleFunc("/", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprint(w, greet.Text())
	})
	if err := http.ListenAndServe(fmt.Sprintf("127.0.0.1:%d", *port), nil); err != nil {
		panic(err)
	}
}
