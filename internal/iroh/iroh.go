package iroh

import (
	"errors"
	"fmt"
	"log"
	"os"

	irohlib "git.coopcloud.tech/decentral1se/iroh-go"
	"github.com/quixvpn/quix/internal/identity"
)

var alpn = []byte("quix-vpn/0")

func GetIrohEndpoint() (*irohlib.Endpoint, error) {
	path, err := identity.KeyPath()
	if err != nil {
		return nil, fmt.Errorf("resolve key path: %w", err)
	}

	preset := irohlib.PresetN0()
	options := irohlib.EndpointOptions{
		Preset: &preset,
		Alpns:  &[][]byte{alpn},
	}

	key, err := identity.LoadSecretKey(path)
	switch {
	case err == nil:
		options.SecretKey = &key
	case errors.Is(err, os.ErrNotExist):
		// first run
	default:
		return nil, fmt.Errorf("load secret key: %w", err)
	}

	endpoint, err := irohlib.EndpointBind(options)
	if err != nil {
		return nil, fmt.Errorf("bind endpoint: %w", err)
	}

	endpoint.Online()

	if options.SecretKey == nil {
		if err := identity.SaveSecretKey(path, endpoint.SecretKey().ToBytes()); err != nil {
			return nil, fmt.Errorf("persist secret key: %w", err)
		}
	}

	return endpoint, nil
}

// connection handling
func AcceptLoop(endpoint *irohlib.Endpoint) {
	for {
		incoming := endpoint.AcceptNext()
		if incoming == nil || *incoming == nil {
			log.Println("endpoint closed, stopping accept loop")
			return
		}

		accepting, err := (*incoming).Accept()
		if err != nil {
			log.Printf("accept failed: %v", err)
			continue
		}

		conn, err := accepting.Connect()
		if err != nil {
			log.Printf("handshake failed: %v", err)
			continue
		}

		go handleConn(conn)
	}
}

func handleConn(conn *irohlib.Connection) {
	defer conn.Close(0, []byte("done"))

	log.Printf("peer connected: %s", conn.RemoteId().String())

	stream, err := conn.AcceptBi()
	if err != nil {
		log.Printf("accept stream failed: %v", err)
		return
	}

	data, err := stream.Recv().ReadToEnd(1024)
	if err != nil {
		log.Printf("read failed: %v", err)
		return
	}

	log.Printf("received: %v", data)

	if err := stream.Send().WriteAll(data); err != nil {
		log.Printf("write failed: %v", err)
		return
	}
	stream.Send().Finish()
}
