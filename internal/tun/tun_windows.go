// ORIGINAL CODE EXTRACTED FROM BLINDSPOT!
// https://github.com/neozmmv/blindspot/blob/master/internal/tun/tun_windows.go

// Windows support requires the forked iroh-go with the x86_64-pc-windows-gnu
// staticlib: https://github.com/neozmmv/iroh-go
//
// Build tag "quixwin" is required because gopls cannot type-check the cgo
// package under the windows configuration, which produces false "undefined"
// errors in the editor.
//
// Cross-compile with:
//   CGO_ENABLED=1 GOOS=windows GOARCH=amd64 CC=x86_64-w64-mingw32-gcc go build -tags=quixwin ./...

//go:build windows && quixwin

package tun

import (
	_ "embed"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"
	"unsafe"

	"golang.org/x/sys/windows"
	wgtun "golang.zx2c4.com/wireguard/tun"
)

var (
	shell32       = windows.NewLazySystemDLL("shell32.dll")
	shellExecuteW = shell32.NewProc("ShellExecuteW")
)

//go:embed wintun.dll
var WintunDLL []byte

// IsAdmin returns true if the current process has administrator privileges.
func IsAdmin() bool {
	return windows.GetCurrentProcessToken().IsElevated()
}

// RelaunchAsAdmin re-launches the current executable with the given args
// via the Windows "runas" verb, triggering a UAC prompt.
func RelaunchAsAdmin(args []string) error {
	exe, err := os.Executable()
	if err != nil {
		return fmt.Errorf("getting executable path: %w", err)
	}

	quoted := make([]string, len(args))
	for i, a := range args {
		if strings.Contains(a, " ") {
			quoted[i] = `"` + a + `"`
		} else {
			quoted[i] = a
		}
	}
	params := strings.Join(quoted, " ")

	verbPtr, _ := windows.UTF16PtrFromString("runas")
	exePtr, _ := windows.UTF16PtrFromString(exe)
	paramsPtr, _ := windows.UTF16PtrFromString(params)

	ret, _, _ := shellExecuteW.Call(
		0,
		uintptr(unsafe.Pointer(verbPtr)),
		uintptr(unsafe.Pointer(exePtr)),
		uintptr(unsafe.Pointer(paramsPtr)),
		0,
		0, // SW_HIDE
	)
	if ret <= 32 {
		return fmt.Errorf("requesting elevation failed (code %d)", ret)
	}
	return nil
}

// Create extracts wintun.dll, creates the TUN adapter, assigns the virtual IP,
// opens the firewall for the virtual network, and sets the network profile to
// Private so Windows file sharing (SMB) works without manual configuration.
func Create(virtualIP string) (Device, error) {
	exePath, err := os.Executable()
	if err != nil {
		return nil, fmt.Errorf("getting executable path: %w", err)
	}
	dllPath := filepath.Join(filepath.Dir(exePath), "wintun.dll")
	if err := os.WriteFile(dllPath, WintunDLL, 0644); err != nil {
		return nil, fmt.Errorf("extracting wintun.dll: %w", err)
	}

	device, err := wgtun.CreateTUN(InterfaceName, 1420)
	if err != nil {
		return nil, fmt.Errorf("creating TUN adapter (run as administrator?): %w", err)
	}

	// wait for the adapter to signal it's up before configuring it
	up := make(chan struct{}, 1)
	var once sync.Once
	go func() {
		for ev := range device.Events() {
			if ev == wgtun.EventUp {
				once.Do(func() { close(up) })
				return
			}
		}
	}()
	select {
	case <-up:
	case <-time.After(5 * time.Second):
	}

	// assign virtual IP
	out, err := exec.Command("netsh", "interface", "ip", "set", "address",
		InterfaceName, "static", virtualIP, NetworkMask).CombinedOutput()
	if err != nil {
		device.Close()
		return nil, fmt.Errorf("assigning IP %s: %w — %s", virtualIP, err, out)
	}

	// MAKES WINDOWS ROUTE THE TRAFFIC THROUGH THE CORRECT INTERFACE
	// BY INDEX
	var ifIdx string
	for range 5 {
		idxOut, _ := exec.Command("netsh", "interface", "ipv4",
			"show", "interfaces").CombinedOutput()

		for _, line := range strings.Split(string(idxOut), "\n") {
			if strings.Contains(strings.ToLower(line), InterfaceName) {
				fields := strings.Fields(line)
				if len(fields) >= 1 {
					ifIdx = fields[0]
					break
				}
			}
		}
		if ifIdx != "" {
			break
		}
		time.Sleep(500 * time.Millisecond)
	}

	if ifIdx == "" {
		device.Close()
		return nil, fmt.Errorf("could not determine %s interface index", InterfaceName)
	}

	// bind route directly to TUN interface index
	if out, err := exec.Command("route", "add", NetworkAddr, "mask", NetworkMask,
		"0.0.0.0", "if", ifIdx, "metric", "1").CombinedOutput(); err != nil {
		device.Close()
		return nil, fmt.Errorf("error routing through the interface: %w — %s", err, out)
	}

	// allow all inbound traffic from the virtual network (HTTP, SMB, RDP, etc.)
	exec.Command("netsh", "advfirewall", "firewall", "delete", "rule", "name="+InterfaceName).Run()
	exec.Command("netsh", "advfirewall", "firewall", "add", "rule",
		"name="+InterfaceName, "dir=in", "action=allow", "remoteip="+NetworkCIDR).Run()

	// set network profile to Private — required for Windows file sharing (SMB)
	exec.Command("powershell", "-NonInteractive", "-Command",
		"Set-NetConnectionProfile -InterfaceAlias "+InterfaceName+" -NetworkCategory Private").Run()

	return device, nil
}
