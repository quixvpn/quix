// ORIGINAL CODE EXTRACTED FROM BLINDSPOT!
// https://github.com/neozmmv/blindspot/blob/master/internal/tun/tun_linux.go

//go:build linux

package tun

import (
	"fmt"
	"os"
	"os/exec"

	wgtun "golang.zx2c4.com/wireguard/tun"
)

// IsAdmin returns true if the current process is running as root.
func IsAdmin() bool {
	return os.Getuid() == 0
}

// Create creates the TUN adapter named "quix" and assigns the virtual IP.
func Create(virtualIP string) (Device, error) {
	device, err := wgtun.CreateTUN("quix", 1420)
	if err != nil {
		return nil, fmt.Errorf("creating TUN adapter (run as root?): %w", err)
	}

	out, err := exec.Command("ip", "addr", "add",
		fmt.Sprintf("%s/%d", virtualIP, PrefixLen), "dev", "quix").CombinedOutput()
	if err != nil {
		device.Close()
		return nil, fmt.Errorf("assigning IP %s: %w — %s", virtualIP, err, out)
	}

	out, err = exec.Command("ip", "link", "set", "quix", "up").CombinedOutput()
	if err != nil {
		device.Close()
		return nil, fmt.Errorf("bringing interface up: %w — %s", err, out)
	}

	return device, nil
}
