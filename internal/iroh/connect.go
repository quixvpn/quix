package iroh

import (
	"fmt"

	irohlib "git.coopcloud.tech/decentral1se/iroh-go"
)

func Ping(endpoint *irohlib.Endpoint, peerID string, msg []byte) ([]byte, error) {
	id, err := irohlib.EndpointIdFromString(peerID)
	if err != nil {
		return nil, fmt.Errorf("parse peer id: %w", err)
	}

	addr := irohlib.NewEndpointAddr(id, nil, nil)

	conn, err := endpoint.Connect(addr, alpn)
	if err != nil {
		return nil, fmt.Errorf("connect: %w", err)
	}
	defer conn.Close(0, []byte("done"))

	stream, err := conn.OpenBi()
	if err != nil {
		return nil, fmt.Errorf("open stream: %w", err)
	}

	if err := stream.Send().WriteAll(msg); err != nil {
		return nil, fmt.Errorf("write: %w", err)
	}
	stream.Send().Finish()

	echo, err := stream.Recv().ReadToEnd(1024)
	if err != nil {
		return nil, fmt.Errorf("read echo: %w", err)
	}

	return echo, nil
}
