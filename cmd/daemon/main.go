// quixd daemon
package main

import (
	"fmt"
	"log"

	"github.com/quixvpn/quix/internal/iroh"
)

func main() {
	endpoint, err := iroh.GetIrohEndpoint()
	if err != nil {
		log.Fatalf("Failed to start quix daemon: %v", err)
	}
	fmt.Println(endpoint.Id())
}
