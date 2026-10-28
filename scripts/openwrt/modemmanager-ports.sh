#!/bin/sh
# Library: only these two physical ports may be reported to our MM process.
# Sourcing the file does not inspect hardware or send any bus commands.
simadmin_mm_port_error() {
	logger -t simadmin "$*" || :
	echo "simadmin: $*" >&2
	return 1
}

simadmin_mm_physical_device() {
	local mm_entry="$1" mm_expected_interface="$2" mm_sysroot="$3"
	local mm_cursor mm_interface=
	mm_cursor=$(readlink -f "$mm_entry") || { simadmin_mm_port_error "Missing sysfs port: $mm_entry"; return 1; }
	[ -d "$mm_cursor" ] || { simadmin_mm_port_error "Missing sysfs port: $mm_entry"; return 1; }
	case "$mm_cursor" in "$mm_sysroot"/devices/*) ;;
		*) simadmin_mm_port_error 'Port resolves outside the physical sysfs device tree'; return 1 ;;
	esac
	while [ "$mm_cursor" != "$mm_sysroot" ] && [ "$mm_cursor" != / ]; do
		if [ -r "$mm_cursor/bInterfaceNumber" ] && [ -z "$mm_interface" ]; then
			mm_interface=$(cat "$mm_cursor/bInterfaceNumber") || return 1
		fi
		if [ -r "$mm_cursor/idVendor" ] && [ -r "$mm_cursor/idProduct" ]; then
			[ "$(cat "$mm_cursor/idVendor")" = 2c7c ] && [ "$(cat "$mm_cursor/idProduct")" = 0900 ] || {
				simadmin_mm_port_error 'Port is not on Quectel USB 2c7c:0900'; return 1
			}
			[ "$mm_interface" = "$mm_expected_interface" ] || {
				simadmin_mm_port_error "Unexpected USB interface $mm_interface; expected $mm_expected_interface"; return 1
			}
			printf '%s\n' "$mm_cursor"
			return 0
		fi
		mm_cursor=$(dirname "$mm_cursor")
	done
	simadmin_mm_port_error 'Cannot identify the physical USB parent'
}

# Alternate roots are arguments for isolated filesystem fixtures only. The
# supervisor always calls simadmin_mm_validate_ports(), with fixed /sys /dev.
simadmin_mm_validate_ports_at() {
	local mm_sysroot mm_devroot="$2" mm_net_physical mm_at_physical
	mm_sysroot=$(readlink -f "$1") || return 1
	[ -c "$mm_devroot/ttyUSB2" ] || { simadmin_mm_port_error 'Missing AT character device ttyUSB2'; return 1; }
	mm_net_physical=$(simadmin_mm_physical_device "$mm_sysroot/class/net/usb0" 00 "$mm_sysroot") || return 1
	mm_at_physical=$(simadmin_mm_physical_device "$mm_sysroot/class/tty/ttyUSB2" 04 "$mm_sysroot") || return 1
	SIMADMIN_MM_PHYSICAL_DEVICE="$mm_net_physical"
	[ "$mm_net_physical" = "$mm_at_physical" ] || {
		simadmin_mm_port_error 'usb0 and ttyUSB2 belong to different physical devices'; return 1
	}
}

simadmin_mm_validate_ports() {
	simadmin_mm_validate_runtime_at /usr/share/dbus-1/system-services/org.freedesktop.ModemManager1.service /var/run/modemmanager/modemmanager.pid || return 1
	simadmin_mm_validate_ports_at /sys /dev
}

# 0 = owned by our child; 1 = not ready; 2 = another owner; 3 = child exited.
simadmin_mm_owner_status() {
	local mm_child="$1" mm_reply mm_owner
	case "$mm_child" in ''|*[!0-9]*|0|1) return 3 ;; esac
	kill -0 "$mm_child" 2>/dev/null || return 3
	mm_reply=$(dbus-send --system --type=method_call --print-reply --reply-timeout=2000 \
		--dest=org.freedesktop.DBus /org/freedesktop/DBus \
		org.freedesktop.DBus.GetConnectionUnixProcessID string:org.freedesktop.ModemManager1 2>/dev/null) || return 1
	mm_owner=$(printf '%s\n' "$mm_reply" | awk '$1 == "uint32" && $2 ~ /^[0-9]+$/ {print $2}')
	[ -n "$mm_owner" ] || return 1
	[ "$mm_owner" = "$mm_child" ] || return 2
}

simadmin_mm_wait_for_owner() {
	local mm_child="$1" mm_attempt=0 mm_status
	while [ "$mm_attempt" -lt 30 ]; do
		if simadmin_mm_owner_status "$mm_child"; then return 0; else mm_status=$?; fi
		case "$mm_status" in
			2) simadmin_mm_port_error 'Another process owns the MM D-Bus name'; return 1 ;;
			3) simadmin_mm_port_error 'MM exited before acquiring its D-Bus name'; return 1 ;;
		esac
		mm_attempt=$((mm_attempt + 1))
		sleep 1
	done
	simadmin_mm_port_error 'MM did not acquire its D-Bus name before the startup deadline'
}

simadmin_mm_report_ports() {
	local mm_child="$1" mm_event
	for mm_event in 'action=add,subsystem=net,name=usb0' 'action=add,subsystem=tty,name=ttyUSB2'; do
		# Revalidate physical ownership before EACH report, including a possible
		# re-enumeration between MM startup and discovery.
		simadmin_mm_validate_ports || return 1
		simadmin_mm_owner_status "$mm_child" || { simadmin_mm_port_error 'Lost ownership of the MM process before port report'; return 1; }
		mmcli --timeout=5 --report-kernel-event="$mm_event" >/dev/null 2>&1 || {
			simadmin_mm_port_error "MM rejected restricted port event: $mm_event"; return 1
		}
	done
}

simadmin_mm_validate_runtime_at() {
	local mm_activation="$1" mm_native_pidfile="$2" mm_activation_exec
	[ ! -s "$mm_native_pidfile" ] || { simadmin_mm_port_error 'Native MM pidfile exists; refusing stock hotplug event injection'; return 1; }
	if [ -f "$mm_activation" ]; then
		mm_activation_exec=$(awk -F= '$1 == "Exec" {print $2}' "$mm_activation")
		[ "$mm_activation_exec" = /bin/false ] || { simadmin_mm_port_error 'Disable unrestricted MM D-Bus activation before using the restricted supervisor'; return 1; }
	fi
}

# All potentially slow CLI calls share the same 60-second discovery deadline.
simadmin_mm_cli_before() {
	local mm_remaining
	mm_remaining=$(($1 - $(date +%s)))
	shift
	[ "$mm_remaining" -gt 0 ] || return 1
	mmcli --timeout="$mm_remaining" "$@"
}

simadmin_mm_json_field() {
	printf '%s\n' "$1" | jsonfilter -e "$2" 2>/dev/null
}

simadmin_mm_matches_target() {
	local mm_json="$1" mm_model mm_primary mm_device mm_ports mm_port mm_has_at=0 mm_has_net=0
	mm_model=$(simadmin_mm_json_field "$mm_json" '@.modem.generic.model') || return 1
	case "$mm_model" in *RG200U*) ;; *) return 1 ;; esac
	mm_primary=$(simadmin_mm_json_field "$mm_json" '@.modem.generic["primary-port"]') || return 1
	[ "$mm_primary" = ttyUSB2 ] || return 1
	mm_device=$(simadmin_mm_json_field "$mm_json" '@.modem.generic.device') || return 1
	[ "$mm_device" = "$SIMADMIN_MM_PHYSICAL_DEVICE" ] || return 1
	mm_ports=$(simadmin_mm_json_field "$mm_json" '@.modem.generic.ports[*]') || return 1
	while IFS= read -r mm_port; do
		case "$mm_port" in
			'ttyUSB2 (at)') [ "$mm_has_at" = 0 ] || return 1; mm_has_at=1 ;;
			'usb0 (net)') [ "$mm_has_net" = 0 ] || return 1; mm_has_net=1 ;;
			*) return 1 ;;
		esac
	done <<EOF_PORTS
$mm_ports
EOF_PORTS
	[ "$mm_has_at" = 1 ] && [ "$mm_has_net" = 1 ]
}

simadmin_mm_enable_target() {
	local mm_child="$1" mm_deadline mm_listing mm_paths mm_object mm_tail mm_json
	local mm_count mm_selected mm_state mm_enabled_target=
	mm_deadline=$(($(date +%s) + 60))
	while [ "$(date +%s)" -lt "$mm_deadline" ]; do
		simadmin_mm_validate_ports || return 1
		simadmin_mm_owner_status "$mm_child" || { simadmin_mm_port_error 'MM owner disappeared during modem initialization'; return 1; }
		if mm_listing=$(simadmin_mm_cli_before "$mm_deadline" -L -J 2>/dev/null); then
			# Empty modem-list is normal while probing; malformed JSON is not.
			[ "$(printf '%s\n' "$mm_listing" | jsonfilter -t '@["modem-list"]' 2>/dev/null)" = array ] || {
				simadmin_mm_port_error 'Invalid MM modem-list response'; return 1
			}
			mm_paths=$(simadmin_mm_json_field "$mm_listing" '@["modem-list"][*]') || mm_paths=
			mm_count=0; mm_selected=; mm_state=
			while IFS= read -r mm_object; do
				[ -n "$mm_object" ] || continue
				case "$mm_object" in /org/freedesktop/ModemManager1/Modem/*) ;;
					*) simadmin_mm_port_error 'Invalid MM object path'; return 1 ;;
				esac
				mm_tail=${mm_object##*/}
				case "$mm_tail" in ''|*[!0-9]*) simadmin_mm_port_error 'Invalid MM modem index'; return 1 ;; esac
				if ! mm_json=$(simadmin_mm_cli_before "$mm_deadline" -m "$mm_object" -J 2>/dev/null); then continue; fi
				if simadmin_mm_matches_target "$mm_json"; then
					mm_count=$((mm_count + 1)); mm_selected="$mm_object"
					mm_state=$(simadmin_mm_json_field "$mm_json" '@.modem.generic.state') || mm_state=
				fi
			done <<EOF_MODEMS
$mm_paths
EOF_MODEMS
			[ "$mm_count" -le 1 ] || { simadmin_mm_port_error 'Multiple modem objects match the restricted ports'; return 1; }
			if [ "$mm_count" = 1 ]; then
				case "$mm_state" in
					enabled|searching|registered|disconnecting|connecting|connected)
						simadmin_mm_validate_ports || return 1
						simadmin_mm_owner_status "$mm_child" || { simadmin_mm_port_error 'MM owner disappeared before readiness'; return 1; }
						return 0 ;;
					disabled)
						if [ "$mm_enabled_target" != "$mm_selected" ]; then
							simadmin_mm_validate_ports || return 1
							simadmin_mm_owner_status "$mm_child" || { simadmin_mm_port_error 'Lost MM owner before Enable'; return 1; }
							simadmin_mm_cli_before "$mm_deadline" -m "$mm_selected" --enable >/dev/null 2>&1 || {
								simadmin_mm_port_error 'Could not enable the bound RG200U modem'; return 1
							}
							mm_enabled_target="$mm_selected"
						fi ;;
					initializing|enabling) ;;
					*) simadmin_mm_port_error "Bound modem is not ready for Messaging (state=$mm_state)"; return 1 ;;
				esac
			fi
		fi
		sleep 1
	done
	simadmin_mm_port_error 'RG200U did not reach enabled state within the 60-second initialization deadline'
}

# Before starting MM, reject any existing owner of its only permitted AT port.
# This reads fd symlink metadata only; it never opens the serial device.
simadmin_mm_require_serial_free_at() {
	local mm_procroot="$1" mm_fd mm_link
	for mm_fd in "$mm_procroot"/[0-9]*/fd/*; do
		[ -L "$mm_fd" ] || continue
		mm_link=$(readlink "$mm_fd" 2>/dev/null) || continue
		[ "$mm_link" != /dev/ttyUSB2 ] || {
			simadmin_mm_port_error "AT port is already open ($mm_fd); refusing to compete with the existing owner"
			return 1
		}
	done
}

simadmin_mm_require_serial_free() {
	simadmin_mm_require_serial_free_at /proc
}
