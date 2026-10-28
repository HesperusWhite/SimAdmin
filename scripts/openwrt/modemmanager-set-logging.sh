#!/bin/sh
# Runs after every MM start, including procd respawns. No serial device is opened.
set -eu
attempt=0
while [ "$attempt" -lt 30 ]; do
	if [ -n "${1:-}" ] && ! kill -0 "$1" 2>/dev/null; then
		logger -t simadmin 'MM exited before logging setup finished.' || :
		exit 1
	fi
	if dbus-send --system --type=method_call --print-reply --reply-timeout=2000 \
		--dest=org.freedesktop.ModemManager1 /org/freedesktop/ModemManager1 \
		org.freedesktop.ModemManager1.SetLogging string:INFO >/dev/null 2>&1; then
		exit 0
	fi
	attempt=$((attempt + 1))
	sleep 1
done
logger -t simadmin 'Could not reduce ModemManager logging to INFO; check its system D-Bus service.' || :
exit 1
