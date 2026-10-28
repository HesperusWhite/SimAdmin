#!/bin/sh
# Every service/bus/logging command is replaced through PATH. No host service,
# D-Bus socket, modem, or network device is contacted by these behavior tests.
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
fixture=$(mktemp -d "${TMPDIR:-/tmp}/simadmin-preflight-test.XXXXXX")
trap 'rm -rf "$fixture"' EXIT INT TERM
mkdir "$fixture/bin"
export SIMADMIN_COMMON="$script_dir/simadmin-common.sh"
export MOCK_TRACE="$fixture/trace" MOCK_MM_STATE="$fixture/mm-state"
export REAL_SLEEP="$(command -v sleep)"
export REAL_JSONFILTER="$(command -v jsonfilter || :)"
cat > "$fixture/bin/ubus" <<'EOF_UBUS'
#!/bin/sh
printf 'ubus %s\n' "$*" >> "$MOCK_TRACE"
if [ "$*" = 'list service' ]; then
	[ "${MOCK_CASE:-absent}" != ubus-fail ] || exit 1
	echo service
	exit 0
fi
[ "$1 $2 $3" = 'call service list' ] || exit 99
case "${MOCK_CASE:-absent}" in
	absent) echo '{}' ;;
	stopped) case "$4" in *'"name":"modem"'*) echo '{"modem":{"instances":{"first":{"running":false},"second":{"running":false}}}}';; *) echo '{}';; esac ;;
	running) echo '{"modem":{"instances":{"first":{"running":false},"second":{"running":true}}}}' ;;
	no-instances) case "$4" in *'"name":"modem"'*) echo '{"modem":{}}';; *) echo '{}';; esac ;;
	empty-instances) case "$4" in *'"name":"modem"'*) echo '{"modem":{"instances":{}}}';; *) echo '{}';; esac ;;
	ubus-fail) echo '{}'; exit 1 ;;
	invalid-json) echo '{not-json' ;;
	array-root) echo '[]' ;;
	wrong-service) echo '{"other":{"instances":{}}}' ;;
	bad-service) echo '{"modem":false}' ;;
	bad-instances) echo '{"modem":{"instances":[]}}' ;;
	bad-instance) echo '{"modem":{"instances":{"first":false}}}' ;;
	missing-running) echo '{"modem":{"instances":{"first":{}}}}' ;;
	string-running) echo '{"modem":{"instances":{"first":{"running":"false"}}}}' ;;
	multiple-services) echo '{"modem":{},"other":{}}' ;;
	*) exit 98 ;;
esac
EOF_UBUS
cat > "$fixture/bin/jsonfilter" <<'EOF_JSON'
#!/usr/bin/env node
// Minimal independent JSONPath mock, matching the real jsonfilter CLI's status
// and type/value output for the expressions the preflight contract permits.
const fs = require('fs');
if (process.env.MOCK_FILTER_FAIL === '1') process.exit(126);
let value;
try { value = JSON.parse(fs.readFileSync(0, 'utf8')); } catch { process.exit(126); }
const [mode, expr] = process.argv.slice(2);
if (!['-e', '-t'].includes(mode) || !/^@(?:\.(?:\*|[a-zA-Z_][a-zA-Z_0-9]*))*$/.test(expr)) process.exit(99);
let values = [value];
for (const key of expr.slice(1).split('.').filter(Boolean)) {
  values = values.flatMap(v => v && typeof v === 'object' && !Array.isArray(v)
    ? key === '*' ? Object.values(v) : Object.hasOwn(v, key) ? [v[key]] : [] : []);
}
if (!values.length) process.exit(1);
const type = v => v === null ? 'null' : Array.isArray(v) ? 'array' : typeof v;
const output = mode === '-t' ? values.map(type).join('\\ ')
  : values.map(v => typeof v === 'string' ? v : JSON.stringify(v)).join('\n');
process.stdout.write(output + '\n');
EOF_JSON
cat > "$fixture/bin/dbus-send" <<'EOF_DBUS'
#!/bin/sh
printf 'dbus-send %s\n' "$*" >> "$MOCK_TRACE"
case "$*" in *NameHasOwner*|*SetLogging*) ;; *) exit 99 ;; esac
case "${MOCK_DBUS_MODE:-success}" in
	success) printf 'method return\n   boolean true\n' ;;
	absent) printf 'method return\n   boolean false\n' ;;
	fail) printf 'method return\n   boolean true\n'; exit 1 ;;
	*) exit 98 ;;
esac
EOF_DBUS
cat > "$fixture/bin/logger" <<'EOF_LOGGER'
#!/bin/sh
printf 'logger %s\n' "$*" >> "$MOCK_TRACE"
exit "${MOCK_LOGGER_EXIT:-0}"
EOF_LOGGER
cat > "$fixture/bin/sleep" <<'EOF_SLEEP'
#!/bin/sh
printf 'sleep %s\n' "$*" >> "$MOCK_TRACE"
exit 0
EOF_SLEEP
chmod +x "$fixture/bin/"*
export PATH="$fixture/bin:$PATH"
run_check() {
	MOCK_CASE="$1" sh -c '. "$SIMADMIN_COMMON"; simadmin_require_no_competing_services' >"$fixture/result" 2>&1
}
for scenario in absent stopped no-instances empty-instances; do
	if ! run_check "$scenario"; then
		cat "$fixture/result" >&2; echo "FAIL: rejected safe service response $scenario" >&2; exit 1
	fi
done
for scenario in running ubus-fail invalid-json array-root wrong-service bad-service bad-instances bad-instance missing-running string-running multiple-services; do
	if run_check "$scenario"; then echo "FAIL: accepted unsafe service response $scenario" >&2; exit 1; fi
done
if MOCK_FILTER_FAIL=1 run_check absent; then echo 'FAIL: ignored JSON parser failure' >&2; exit 1; fi
# If installed, also verify the mock contract against the real local JSON
# parser. ubus/dbus/logger remain mocks; the parser sees fixture strings only.
if [ -n "$REAL_JSONFILTER" ]; then
	for scenario in absent stopped no-instances empty-instances; do
		MOCK_CASE="$scenario" sh -c 'jsonfilter() { "$REAL_JSONFILTER" "$@"; }; . "$SIMADMIN_COMMON"; simadmin_require_stopped_service modem'
	done
	for scenario in running invalid-json array-root wrong-service bad-instances missing-running string-running; do
		if MOCK_CASE="$scenario" sh -c 'jsonfilter() { "$REAL_JSONFILTER" "$@"; }; . "$SIMADMIN_COMMON"; simadmin_require_stopped_service modem' >"$fixture/result" 2>&1; then
			echo "FAIL: real JSON parser accepted unsafe fixture $scenario" >&2; exit 1
		fi
	done
fi

if MOCK_CASE=ubus-fail sh -c '. "$SIMADMIN_COMMON"; simadmin_require_service_bus' >"$fixture/result" 2>&1; then
	echo 'FAIL: accepted unavailable procd bus' >&2; exit 1
fi
# The external-MM path must apply the same guard to the legacy dialer.
if MOCK_CASE=ubus-fail sh -c '. "$SIMADMIN_COMMON"; simadmin_require_stopped_service modem' >"$fixture/result" 2>&1; then
	echo 'FAIL: external MM bypassed unavailable legacy service state' >&2; exit 1
fi
MOCK_DBUS_MODE=success sh -c '. "$SIMADMIN_COMMON"; simadmin_mm_has_owner'
for mode in absent fail; do
	if MOCK_DBUS_MODE="$mode" sh -c '. "$SIMADMIN_COMMON"; simadmin_mm_has_owner'; then
		echo "FAIL: accepted invalid MM ownership $mode" >&2; exit 1
	fi
done
: > "$MOCK_TRACE"
MOCK_DBUS_MODE=success sh "$script_dir/modemmanager-set-logging.sh"
[ "$(grep -c 'SetLogging' "$MOCK_TRACE")" -eq 1 ]
: > "$MOCK_TRACE"
if MOCK_DBUS_MODE=fail sh "$script_dir/modemmanager-set-logging.sh"; then
	echo 'FAIL: logging helper accepted a failed D-Bus call' >&2; exit 1
fi
[ "$(grep -c 'SetLogging' "$MOCK_TRACE")" -eq 30 ]
grep -q 'Could not reduce ModemManager logging' "$MOCK_TRACE"
# Even a broken logging daemon cannot turn a preflight failure into success.
if MOCK_LOGGER_EXIT=1 run_check ubus-fail; then echo 'FAIL: ignored ubus error when logger failed' >&2; exit 1; fi
# Wrapper failure must terminate only its own fake MM child, preventing debug
# logging from continuing after SetLogging fails. No real MM binary is used.
cat > "$fixture/fake-mm" <<'EOF_MM'
#!/bin/sh
trap 'printf "stopped\n" >> "$MOCK_MM_STATE"; exit 0' TERM INT
printf 'started\n' >> "$MOCK_MM_STATE"
while :; do "$REAL_SLEEP" 0.05; done
EOF_MM
cat > "$fixture/fail-logging" <<'EOF_FAIL_LOGGING'
#!/bin/sh
attempt=0
while [ ! -f "$MOCK_MM_STATE" ] && [ "$attempt" -lt 20 ]; do
	"$REAL_SLEEP" 0.01
	attempt=$((attempt + 1))
done
exit 1
EOF_FAIL_LOGGING
cat > "$fixture/no-hardware-ports.sh" <<'EOF_PORTS_MOCK'
simadmin_mm_validate_ports() { :; }
simadmin_mm_require_serial_free() { :; }
simadmin_mm_wait_for_owner() { :; }
simadmin_mm_report_ports() { echo 'Unexpected report after logging failure' >&2; return 1; }
EOF_PORTS_MOCK
chmod +x "$fixture/fake-mm" "$fixture/fail-logging"
node - "$script_dir/modemmanager-wrapper.sh" "$fixture/fake-mm" "$fixture/fail-logging" "$fixture/no-hardware-ports.sh" <<'EOF_WRAPPER_TEST'
const { spawnSync } = require('child_process');
const result = spawnSync('sh', process.argv.slice(2), { timeout: 3000, stdio: 'inherit' });
if (result.error || result.status !== 1) {
  process.stderr.write('FAIL: wrapper did not fail promptly after logging setup failed\n');
  process.exit(1);
}
EOF_WRAPPER_TEST
grep -qx started "$MOCK_MM_STATE"
grep -qx stopped "$MOCK_MM_STATE"
echo 'OpenWrt preflight behavior tests passed (competition, malformed/failed bus, owner, logging failure).'
