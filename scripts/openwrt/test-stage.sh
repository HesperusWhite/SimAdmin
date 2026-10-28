#!/bin/sh
# Packaging guard tests only: no hardware, system bus, or live service access.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/simadmin-stage-test.XXXXXX")
trap 'rm -rf "$fixture"' EXIT INT TERM
for source in "$script_dir/"*.sh "$script_dir/"*.init; do sh -n "$source"; done
mkdir "$fixture/frontend"
printf '<html>stage fixture</html>\n' > "$fixture/frontend/index.html"
# ELF marker is sufficient for this packaging fixture; never execute it.
printf '\177ELFfixture\n' > "$fixture/simadmin"
chmod 755 "$fixture/simadmin"
"$script_dir/stage.sh" "$fixture/review" "$fixture/simadmin" "$fixture/frontend" >/dev/null
[ -f "$fixture/review/etc/init.d/simadmin" ]
[ -f "$fixture/review/etc/init.d/simadmin-modemmanager" ]
[ -f "$fixture/review/usr/lib/simadmin/modemmanager-wrapper.sh" ]
[ -f "$fixture/review/usr/lib/simadmin/modemmanager-ports.sh" ]
[ -f "$fixture/review/lib/udev/rules.d/78-mm-simadmin-rg200u.rules" ]
grep -qx 'Exec=/bin/false' "$fixture/review/usr/share/dbus-1/system-services/org.freedesktop.ModemManager1.service"
[ -f "$fixture/review/opt/simadmin/www/index.html" ]
[ ! -d "$fixture/review/etc/rc.d" ]
[ "$(grep -c "option enabled '0'" "$fixture/review/etc/config/simadmin")" -eq 2 ]
(cd "$fixture/review" && sha256sum -c SHA256SUMS >/dev/null)
printf 'keep\n' > "$fixture/review/keep"
if "$script_dir/stage.sh" "$fixture/review" "$fixture/simadmin" "$fixture/frontend" >/dev/null 2>&1; then
	echo 'FAIL: staging overwrote an existing directory' >&2; exit 1
fi
[ "$(cat "$fixture/review/keep")" = keep ]
# /proc is unwritable even if the guard regresses; require the explicit guard
# message, rather than accepting an unrelated mkdir failure as a passing test.
if "$script_dir/stage.sh" "/proc/simadmin-stage-test.$$" "$fixture/simadmin" "$fixture/frontend" >"$fixture/guard.log" 2>&1; then
	echo 'FAIL: staging accepted a live system destination' >&2; exit 1
fi
grep -q 'Refusing a live system destination' "$fixture/guard.log"
printf '#!/bin/sh\nexit 0\n' > "$fixture/not-elf"
chmod 755 "$fixture/not-elf"
if "$script_dir/stage.sh" "$fixture/rejected" "$fixture/not-elf" "$fixture/frontend" >/dev/null 2>&1; then
	echo 'FAIL: staging accepted a shell script as the compiled app' >&2; exit 1
fi
[ ! -e "$fixture/rejected" ]
"$script_dir/test-preflight.sh"
"$script_dir/test-modemmanager.sh"
echo 'OpenWrt packaging tests passed (syntax, disabled defaults, checksums, output guards).'
