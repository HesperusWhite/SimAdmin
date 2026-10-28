#!/bin/sh
# One MM owner, no automatic scan, no stock monitor/cache replay. The optional
# helper/library arguments support isolated tests; the procd template uses the
# fixed installed defaults.
set -eu
. "${3:-/usr/lib/simadmin/modemmanager-ports.sh}"
mm_pid=
logging_pid=
cleanup() {
	trap - EXIT INT TERM
	[ -z "$logging_pid" ] || kill "$logging_pid" 2>/dev/null || :
	[ -z "$mm_pid" ] || kill "$mm_pid" 2>/dev/null || :
	[ -z "$logging_pid" ] || wait "$logging_pid" 2>/dev/null || :
	[ -z "$mm_pid" ] || wait "$mm_pid" 2>/dev/null || :
}
trap cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
simadmin_mm_validate_ports
simadmin_mm_require_serial_free
"${1:-/usr/sbin/ModemManager}" --debug --no-auto-scan --test-no-qrtr --filter-policy=ALLOWLIST-ONLY &
mm_pid=$!
simadmin_mm_wait_for_owner "$mm_pid"
# Lower logging BEFORE reporting ports, so discovery runs with INFO logging.
"${2:-/usr/lib/simadmin/modemmanager-set-logging.sh}" "$mm_pid" &
logging_pid=$!
if wait "$logging_pid"; then
	logging_pid=
else
	logging_pid=
	exit 1
fi
simadmin_mm_report_ports "$mm_pid"
simadmin_mm_enable_target "$mm_pid"
logger -t simadmin 'RG200U initialization ready (enabled, usb0/ttyUSB2)' || :
set +e
wait "$mm_pid"
result=$?
set -e
mm_pid=
exit "$result"
