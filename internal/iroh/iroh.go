package iroh

import (
	"errors"
	"fmt"
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
