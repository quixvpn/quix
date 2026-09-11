package ipc

import (
	"encoding/json"
	"fmt"
	"net"
	"os"

	irohlib "git.coopcloud.tech/decentral1se/iroh-go"
	"github.com/quixvpn/quix/internal/iroh"
)

func Serve(endpoint *irohlib.Endpoint) error {
	path := SocketPath()
	os.Remove(path) // clean up a stale socket from a previous run

	listener, err := net.Listen("unix", path)
	if err != nil {
		return fmt.Errorf("listen on socket: %w", err)
	}
	defer listener.Close()

	// only the owner can talk to the daemon
	// 0660 FOR QUIX GROUP AS SYSTEM SERVICE !!
	// docker-like service daemon
	if err := os.Chmod(path, 0600); err != nil {
		return fmt.Errorf("restrict socket permissions: %w", err)
	}

	for {
		conn, err := listener.Accept()
		if err != nil {
			return fmt.Errorf("accept socket conn: %w", err)
		}
		go handleRequest(conn, endpoint)
	}
}

func handleRequest(conn net.Conn, endpoint *irohlib.Endpoint) {
	defer conn.Close()

	var req Request
	if err := json.NewDecoder(conn).Decode(&req); err != nil {
		return
	}

	var resp Response
	switch req.Cmd {
	case "ping":
		echo, err := iroh.Ping(endpoint, req.Peer, []byte(req.Msg))
		if err != nil {
			resp.Error = err.Error()
		} else {
			resp.OK = true
			resp.Echo = string(echo)
		}
	default:
		resp.Error = "unknown command: " + req.Cmd
	}

	json.NewEncoder(conn).Encode(resp)
}
