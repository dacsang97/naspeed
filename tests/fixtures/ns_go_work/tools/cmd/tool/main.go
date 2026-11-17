package main

import (
	"flag"
	"fmt"
	"net/http"
	"os"
)

func main() {
	port := flag.Int("port", 19083, "listen port")
	flag.Parse()
	http.HandleFunc("/pid", func(w http.ResponseWriter, _ *http.Request) {
		fmt.Fprint(w, os.Getpid())
	})
	if err := http.ListenAndServe(fmt.Sprintf("127.0.0.1:%d", *port), nil); err != nil {
		panic(err)
	}
}
