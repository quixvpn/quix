package commands

import (
	"github.com/spf13/cobra"
)

var rootCmd = &cobra.Command{
	Use:   "quix",
	Short: "P2P mesh VPN over QUIC",
	Long:  "Quix connects your machines into a private mesh network over QUIC.",
}

func Execute() error {
	return rootCmd.Execute()
}
