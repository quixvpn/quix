module github.com/quixvpn/quix

go 1.26.1

require (
	git.coopcloud.tech/decentral1se/iroh-go v0.0.0-20260830120307-d6233351aba3
	github.com/spf13/cobra v1.10.2
	golang.org/x/sys v0.32.0
	golang.zx2c4.com/wireguard v0.0.0-20260522210424-ecfc5a8d5446
)

require (
	github.com/inconshreveable/mousetrap v1.1.0 // indirect
	github.com/spf13/pflag v1.0.9 // indirect
	golang.org/x/net v0.39.0 // indirect
	golang.zx2c4.com/wintun v0.0.0-20230126152724-0fa3db229ce2 // indirect
)

replace git.coopcloud.tech/decentral1se/iroh-go => github.com/neozmmv/iroh-go v0.1.0
