#!/usr/bin/env bash
# Installs quix and quixd, puts them on PATH, and runs the daemon as a systemd
# service.
#
#   sudo ./scripts/install.sh            # from the latest GitHub release
#   sudo ./scripts/install.sh --local    # from ./target/release (builds if needed)
#   sudo ./scripts/install.sh --uninstall
#
# Override with INSTALL_DIR (default /usr/local/bin) and REPO.
set -euo pipefail

REPO=${REPO:-quixvpn/quix}
INSTALL_DIR=${INSTALL_DIR:-/usr/local/bin}
UNIT_DIR=${UNIT_DIR:-/etc/systemd/system}
SERVICE=quixd

die() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

[[ $EUID -eq 0 ]] || die "needs root (installs a system service): sudo $0 $*"
command -v systemctl >/dev/null || die "systemd not found; this script only handles systemd hosts"

uninstall() {
	info "stopping $SERVICE"
	systemctl disable --now "$SERVICE" 2>/dev/null || true
	rm -f "$UNIT_DIR/$SERVICE.service"
	systemctl daemon-reload
	rm -f "$INSTALL_DIR/quix" "$INSTALL_DIR/quixd"
	# Deliberately keeps /var/lib/quix: it holds the identity key, and losing
	# that means losing the node's addresses and its place in every network.
	info "removed. identity and roster kept in /var/lib/quix"
	exit 0
}

[[ ${1:-} == --uninstall ]] && uninstall

STAGE=$(mktemp -d)
trap 'rm -rf "$STAGE"' EXIT

if [[ ${1:-} == --local ]]; then
	ROOT=$(cd "$(dirname "$0")/.." && pwd)
	if [[ ! -x $ROOT/target/release/quixd ]]; then
		info "building release binaries"
		( cd "$ROOT" && cargo build --release --workspace )
	fi
	cp "$ROOT/target/release/quix" "$ROOT/target/release/quixd" "$STAGE/"
	cp "$ROOT/packaging/quixd.service" "$STAGE/"
else
	case "$(uname -m)" in
		x86_64|amd64) asset=quix-linux-x86_64.tar.gz ;;
		aarch64|arm64) asset=quix-linux-aarch64.tar.gz ;;
		*) die "no prebuilt binary for $(uname -m); use --local to build from source" ;;
	esac

	command -v curl >/dev/null || die "curl is required"
	info "downloading $asset from $REPO"
	base="https://github.com/$REPO/releases/latest/download"
	curl -fsSL "$base/$asset" -o "$STAGE/$asset" || die "download failed (is there a release yet? try --local)"
	curl -fsSL "$base/$asset.sha256" -o "$STAGE/$asset.sha256" || die "checksum download failed"

	info "verifying checksum"
	( cd "$STAGE" && sha256sum -c "$asset.sha256" >/dev/null ) || die "checksum mismatch — refusing to install"

	tar -xzf "$STAGE/$asset" -C "$STAGE" --strip-components=1
fi

[[ -f $STAGE/quix && -f $STAGE/quixd ]] || die "binaries missing from the package"

# Overwriting a running binary truncates the file underneath the process, and
# a unit rewritten under a live service is simply ignored. Stop first.
if systemctl is-active --quiet "$SERVICE" 2>/dev/null; then
	info "stopping $SERVICE to replace it"
	systemctl stop "$SERVICE"
fi

info "installing to $INSTALL_DIR"
install -d "$INSTALL_DIR"
install -m755 "$STAGE/quix" "$STAGE/quixd" "$INSTALL_DIR/"

info "installing the $SERVICE service"
# ExecStart is absolute in the unit, so point it at wherever we just installed.
sed "s|^ExecStart=.*|ExecStart=$INSTALL_DIR/quixd|" "$STAGE/quixd.service" > "$UNIT_DIR/$SERVICE.service"
chmod 644 "$UNIT_DIR/$SERVICE.service"

systemctl daemon-reload
systemctl enable "$SERVICE"
# `enable --now` only starts a stopped unit, so it would silently leave an
# upgrade running the old code. Restart says what we actually mean.
systemctl reset-failed "$SERVICE" 2>/dev/null || true
systemctl restart "$SERVICE"

# systemd reports the unit active as soon as the process is up, which is before
# the daemon has picked a relay (up to 10s), brought the TUN up and bound its
# socket. Wait for the socket itself, which is the last thing it does.
info "waiting for the daemon to come up"
socket=$(systemctl show -p Environment --value "$SERVICE" |
	tr ' ' '\n' | sed -n 's/^QUIX_SOCKET=//p')
socket=${socket:-/run/quix/quixd.sock}

ready=
for _ in $(seq 1 40); do
	if ! systemctl is-active --quiet "$SERVICE"; then
		echo
		echo "the service stopped while starting. recent log:" >&2
		journalctl -u "$SERVICE" -n 20 --no-pager >&2
		exit 1
	fi
	if [[ -S $socket ]]; then ready=1; break; fi
	sleep 0.5
done

if [[ -z $ready ]]; then
	echo
	echo "the daemon did not bind $socket within 20s. recent log:" >&2
	journalctl -u "$SERVICE" -n 20 --no-pager >&2
	exit 1
fi

# Whoever invoked sudo is the obvious operator: they just installed it, and
# without this every command would need root from here on.
if [[ -n ${SUDO_USER:-} ]]; then
	info "making $SUDO_USER the operator"
	"$INSTALL_DIR/quix" set-operator "$SUDO_USER" || \
		echo "warning: could not set the operator; run 'sudo quix set-operator <user>' yourself" >&2
fi

echo
info "installed"
"$INSTALL_DIR/quix" status || true
echo
echo "  quix status          see this node and its peers"
echo "  quix create <name>   start a network"
echo "  quix join <code>     join one"
echo "  journalctl -u $SERVICE -f   daemon log"
if [[ $PATH != *"$INSTALL_DIR"* ]]; then
	echo
	echo "note: $INSTALL_DIR is not on your PATH"
fi
