#!/bin/sh
# Read-only checks shared by the two optional procd services.
simadmin_error() {
	logger -t simadmin "$*" || :
	echo "simadmin: $*" >&2
	return 1
}

simadmin_require_bus() {
	[ -S /var/run/dbus/system_bus_socket ] || simadmin_error 'System D-Bus is unavailable; start the distribution dbus service first.'
}

simadmin_mm_has_owner() {
	local simadmin_reply
	simadmin_reply=$(dbus-send --system --print-reply --reply-timeout=2000 \
		--dest=org.freedesktop.DBus /org/freedesktop/DBus \
		org.freedesktop.DBus.NameHasOwner string:org.freedesktop.ModemManager1 2>/dev/null) || return 1
	printf '%s\n' "$simadmin_reply" | grep -q 'boolean true'
}

simadmin_require_service_bus() {
	ubus list service >/dev/null 2>&1 || simadmin_error 'procd service interface is unavailable on ubus.'
}

# Reject a running service AND an unavailable/unparseable answer. Only a proven
# stopped/absent service permits MM to take ownership of the serial ports.
simadmin_require_stopped_service() {
	local simadmin_service="$1" simadmin_reply simadmin_type simadmin_json
	local simadmin_instances simadmin_instance simadmin_running simadmin_status
	case "$simadmin_service" in modem|modemmanager|ModemManager) ;;
		*) simadmin_error 'Unsupported competing service name'; return 1 ;;
	esac
	simadmin_reply=$(ubus call service list "{\"name\":\"$simadmin_service\"}" 2>/dev/null) || {
		simadmin_error "Cannot determine whether service $simadmin_service is running (ubus failed)."
		return 1
	}
	simadmin_type=$(printf '%s\n' "$simadmin_reply" | jsonfilter -t '@' 2>/dev/null) || {
		simadmin_error "Invalid service response for $simadmin_service."
		return 1
	}
	[ "$simadmin_type" = object ] || { simadmin_error 'Service response must be a JSON object.'; return 1; }
	simadmin_json=$(printf '%s\n' "$simadmin_reply" | jsonfilter -e '@' 2>/dev/null) || {
		simadmin_error 'Cannot parse service response.'; return 1
	}
	# procd returns an empty root object for an unregistered service.
	[ "$(printf '%s' "$simadmin_json" | tr -d ' \t\r\n')" != '{}' ] || return 0
	simadmin_type=$(printf '%s\n' "$simadmin_reply" | jsonfilter -t '@.*' 2>/dev/null) || {
		simadmin_error 'Cannot inspect service response.'; return 1
	}
	[ "$simadmin_type" = object ] || { simadmin_error 'Expected exactly one named service object.'; return 1; }
	simadmin_type=$(printf '%s\n' "$simadmin_reply" | jsonfilter -t "@.$simadmin_service" 2>/dev/null) || {
		simadmin_error 'Service response does not match the requested name.'; return 1
	}
	[ "$simadmin_type" = object ] || { simadmin_error 'Invalid named service object.'; return 1; }
	if simadmin_type=$(printf '%s\n' "$simadmin_reply" | jsonfilter -t "@.$simadmin_service.instances" 2>/dev/null); then
		[ "$simadmin_type" = object ] || { simadmin_error 'Service instances must be a JSON object.'; return 1; }
	else
		simadmin_status=$?
		# procd service_dump() omits instances when the service has none.
		[ "$simadmin_status" -eq 1 ] && return 0
		simadmin_error 'Cannot inspect service instances.'; return 1
	fi
	if simadmin_instances=$(printf '%s\n' "$simadmin_reply" | jsonfilter -e "@.$simadmin_service.instances.*" 2>/dev/null); then
		:
	else
		simadmin_status=$?
		[ "$simadmin_status" -eq 1 ] && [ -z "$simadmin_instances" ] && return 0
		simadmin_error 'Cannot read service instances.'; return 1
	fi
	while IFS= read -r simadmin_instance; do
		simadmin_type=$(printf '%s\n' "$simadmin_instance" | jsonfilter -t '@' 2>/dev/null) || {
			simadmin_error 'Invalid service instance.'; return 1
		}
		[ "$simadmin_type" = object ] || { simadmin_error 'Service instance must be a JSON object.'; return 1; }
		simadmin_type=$(printf '%s\n' "$simadmin_instance" | jsonfilter -t '@.running' 2>/dev/null) || {
			simadmin_error 'Service instance has no running state.'; return 1
		}
		[ "$simadmin_type" = boolean ] || { simadmin_error 'Service running state must be boolean.'; return 1; }
		simadmin_running=$(printf '%s\n' "$simadmin_instance" | jsonfilter -e '@.running' 2>/dev/null) || {
			simadmin_error 'Cannot read service running state.'; return 1
		}
		case "$simadmin_running" in
			false) ;;
			true) simadmin_error "Competing service $simadmin_service is running; stop and disable it before using SimAdmin."; return 1 ;;
			*) simadmin_error 'Invalid service running value.'; return 1 ;;
		esac
	done <<EOF_INSTANCES
$simadmin_instances
EOF_INSTANCES
}

simadmin_require_no_competing_services() {
	local simadmin_service
	# Never stop the legacy dialer or another MM supervisor from this helper.
	for simadmin_service in modem modemmanager ModemManager; do
		simadmin_require_stopped_service "$simadmin_service" || return 1
	done
}

# Bounded boot ordering: dbus and MM may still be acquiring their names.
simadmin_wait_for_mm() {
	local simadmin_attempt=0
	while [ "$simadmin_attempt" -lt 30 ]; do
		simadmin_mm_has_owner && return 0
		simadmin_attempt=$((simadmin_attempt + 1))
		sleep 1
	done
	simadmin_error 'ModemManager has not acquired its system D-Bus name.'
}
