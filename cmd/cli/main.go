package main

import (
	"log"

	"github.com/quixvpn/quix/cmd/cli/commands"
)

func main() {
	if err := commands.Execute(); err != nil {
		log.Fatal(err)
	}
}
