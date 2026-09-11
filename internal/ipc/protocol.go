package ipc

import (
	"os"
	"path/filepath"
)

type Request struct {
	Cmd  string `json:"cmd"`
	Peer string `json:"peer,omitempty"`
	Msg  string `json:"msg,omitempty"`
}

type Response struct {
	OK    bool   `json:"ok"`
	Echo  string `json:"echo,omitempty"`
	Error string `json:"error,omitempty"`
}

func SocketPath() string {
	if custom := os.Getenv("QUIX_SOCKET"); custom != "" {
		return custom
	}
	if dir := os.Getenv("XDG_RUNTIME_DIR"); dir != "" {
		return filepath.Join(dir, "quix.sock")
	}
	return "/tmp/quix.sock"
}
