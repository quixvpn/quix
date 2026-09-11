package main

import (
	"flag"
	"fmt"
	"log"

	"github.com/quixvpn/quix/internal/iroh"
)

func main() {
	peerID := flag.String("peer", "", "peer endpoint id")
	flag.Parse()

	if *peerID == "" {
		log.Fatal("usage: quix -peer <endpoint-id>")
	}

	endpoint, err := iroh.GetIrohEndpoint()
	if err != nil {
		log.Fatalf("failed to bind: %v", err)
	}

	echo, err := iroh.Ping(endpoint, *peerID, []byte("hello from quix"))
	if err != nil {
		log.Fatalf("ping failed: %v", err)
	}

	fmt.Println("echo:", string(echo))
}
