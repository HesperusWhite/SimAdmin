#!/bin/sh
# Build an inspectable filesystem tree. Never copies to live /etc or /opt,
# invokes an init script, enables services, or runs the supplied binary.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
repo_dir=$(CDPATH= cd -- "$script_dir/../.." && pwd)
output=${1:-"$repo_dir/build/openwrt-stage"}
binary=${2:-"$repo_dir/target/aarch64-unknown-linux-musl/release/simadmin"}
frontend=${3:-"$repo_dir/frontend/dist"}
if [ "$#" -gt 3 ]; then
	echo 'Usage: stage.sh [NEW_OUTPUT_DIRECTORY [COMPILED_BINARY [FRONTEND_DIST]]]' >&2
	exit 2
fi
case "$output" in /*) ;; *) output="$PWD/$output" ;; esac
# Resolve only the existing parent. Require a new final directory, preventing
# accidental replacement of an installed tree or any earlier review artifacts.
parent=$(readlink -f "$(dirname -- "$output")") || { echo 'Output parent must exist (or have an existing parent).' >&2; exit 2; }
output="$parent/$(basename -- "$output")"
case "$output" in /|/etc|/etc/*|/usr|/usr/*|/opt|/opt/*|/var|/var/*|/bin|/sbin|/lib|/proc/*|/sys/*|/dev/*)
	echo "Refusing a live system destination: $output" >&2
	exit 2
esac
[ ! -e "$output" ] && [ ! -L "$output" ] || { echo "Output already exists: $output" >&2; exit 2; }
[ -f "$binary" ] && [ -x "$binary" ] || { echo "Missing compiled executable: $binary" >&2; exit 2; }
[ -f "$frontend/index.html" ] || { echo "Missing frontend build: $frontend/index.html" >&2; exit 2; }
magic=$(hexdump -v -n 4 -e '1/1 "%02x"' "$binary")
[ "$magic" = 7f454c46 ] || { echo 'Binary must be a compiled ELF executable.' >&2; exit 2; }
mkdir -p "$parent"
mkdir "$output"
mkdir -p "$output/opt/simadmin/www" "$output/etc/init.d" "$output/etc/config" "$output/usr/lib/simadmin" "$output/lib/udev/rules.d" "$output/usr/share/dbus-1/system-services"
cp "$binary" "$output/opt/simadmin/simadmin"
cp -R "$frontend/." "$output/opt/simadmin/www/"
cp "$script_dir/simadmin.init" "$output/etc/init.d/simadmin"
cp "$script_dir/simadmin-modemmanager.init" "$output/etc/init.d/simadmin-modemmanager"
cp "$script_dir/simadmin.config" "$output/etc/config/simadmin"
cp "$script_dir/simadmin-common.sh" "$output/usr/lib/simadmin/openwrt-common.sh"
cp "$script_dir/modemmanager-wrapper.sh" "$output/usr/lib/simadmin/"
cp "$script_dir/modemmanager-ports.sh" "$output/usr/lib/simadmin/"
cp "$script_dir/78-mm-simadmin-rg200u.rules" "$output/lib/udev/rules.d/"
cp "$script_dir/org.freedesktop.ModemManager1.service" "$output/usr/share/dbus-1/system-services/"
cp "$script_dir/modemmanager-set-logging.sh" "$output/usr/lib/simadmin/"
cp "$repo_dir/docs/openwrt.md" "$output/README.md"
chmod 755 "$output/opt/simadmin/simadmin" "$output/etc/init.d/simadmin" "$output/etc/init.d/simadmin-modemmanager" "$output/usr/lib/simadmin/"*.sh
chmod 600 "$output/etc/config/simadmin"
(
	cd "$output"
	find opt etc usr lib -type f | LC_ALL=C sort | while IFS= read -r file; do sha256sum "$file"; done > SHA256SUMS
)
printf 'OpenWrt review tree: %s\nServices remain disabled; no files were installed.\n' "$output"
