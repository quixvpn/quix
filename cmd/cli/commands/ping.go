package commands

import (
	"encoding/json"
	"fmt"
	"net"

	"github.com/quixvpn/quix/internal/ipc"
	"github.com/spf13/cobra"
)

var pingCmd = &cobra.Command{
	Use:   "ping <endpoint-id>",
	Short: "Send a test message to a peer",
	Args:  cobra.ExactArgs(1),
	RunE: func(cmd *cobra.Command, args []string) error {
		resp, err := sendRequest(ipc.Request{
			Cmd:  "ping",
			Peer: args[0],
			Msg:  "hello from quix",
		})
		if err != nil {
			return err
		}
		if !resp.OK {
			return fmt.Errorf("ping failed: %s", resp.Error)
		}

		fmt.Println("echo:", resp.Echo)
		return nil
	},
}

func init() {
	rootCmd.AddCommand(pingCmd)
}

func sendRequest(req ipc.Request) (*ipc.Response, error) {
	conn, err := net.Dial("unix", ipc.SocketPath())
	if err != nil {
		return nil, fmt.Errorf("is quixd running? %w", err)
	}
	defer conn.Close()

	if err := json.NewEncoder(conn).Encode(req); err != nil {
		return nil, fmt.Errorf("send request: %w", err)
	}

	var resp ipc.Response
	if err := json.NewDecoder(conn).Decode(&resp); err != nil {
		return nil, fmt.Errorf("read response: %w", err)
	}

	return &resp, nil
}
