// quixd daemon
package main

import (
	"fmt"
	"log"

	"github.com/quixvpn/quix/internal/ipc"
	"github.com/quixvpn/quix/internal/iroh"
)

func main() {
	endpoint, err := iroh.GetIrohEndpoint()
	if err != nil {
		log.Fatalf("Failed to start quix daemon: %v", err)
	}

	fmt.Println("quixd listening, id:", endpoint.Id())

	go iroh.AcceptLoop(endpoint) // handles incoming peer connections

	if err := ipc.Serve(endpoint); err != nil {
		log.Fatalf("socket server failed: %v", err)
	}
}
