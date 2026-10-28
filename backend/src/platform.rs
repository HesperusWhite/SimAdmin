//! Host service control without installing or enabling services.

use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use tokio::process::Command;

pub fn is_openwrt() -> bool {
    detect_openwrt(
        std::env::var("SIMADMIN_PLATFORM").ok().as_deref(),
        Path::new("/etc/openwrt_release").exists(),
    )
}

fn detect_openwrt(override_value: Option<&str>, has_release_file: bool) -> bool {
    match override_value {
        Some("openwrt") => true,
        Some("systemd") => false,
        _ => has_release_file,
    }
}

fn validate_service_name(name: &str) -> Result<()> {
    if !name
        .bytes()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        || name.starts_with('.')
    {
        bail!("Invalid service name: {name}");
    }
    Ok(())
}

/// Resolve the init script; this never executes or creates a host service.
pub fn openwrt_service_name(name: &str) -> Result<String> {
    let configured = std::env::var("SIMADMIN_MODEM_SERVICE").ok();
    resolve_openwrt_service_name(name, configured.as_deref())
}

fn resolve_openwrt_service_name(name: &str, modem_service: Option<&str>) -> Result<String> {
    validate_service_name(name)?;
    let name = name.strip_suffix(".service").unwrap_or(name);
    let resolved = match name {
        "ModemManager" => modem_service.unwrap_or("simadmin-modemmanager"),
        "simadmin" => "simadmin",
        _ => bail!("OpenWrt service control is restricted to SimAdmin and ModemManager"),
    };
    validate_service_name(resolved)?;
    if name == "ModemManager"
        && !matches!(
            resolved,
            "simadmin-modemmanager" | "modemmanager" | "ModemManager"
        )
    {
        bail!("Unsupported OpenWrt ModemManager service: {resolved}");
    }
    Ok(resolved.to_owned())
}

async fn service_action(name: &str, action: &str) -> Result<()> {
    validate_service_name(name)?;
    let mut command = if is_openwrt() {
        Command::new(format!("/etc/init.d/{}", openwrt_service_name(name)?))
    } else {
        Command::new("systemctl")
    };
    if is_openwrt() {
        command.arg(action);
    } else {
        command.args([action, name]);
    }
    command.kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(45), command.output())
        .await
        .context("Service command timed out")?
        .context("Failed to execute service command")?;
    if !output.status.success() {
        return Err(anyhow!(
            "Service {name} {action} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

pub async fn restart_service(name: &str) -> Result<()> {
    service_action(name, "restart").await
}

pub async fn start_service(name: &str) -> Result<()> {
    service_action(name, "start").await
}

pub async fn stop_service(name: &str) -> Result<()> {
    service_action(name, "stop").await
}

pub async fn service_is_active(name: &str) -> Result<bool> {
    validate_service_name(name)?;
    let mut command = if is_openwrt() {
        let mut command = Command::new(format!("/etc/init.d/{}", openwrt_service_name(name)?));
        command.arg("status");
        command
    } else {
        let mut command = Command::new("systemctl");
        command.args(["is-active", "--quiet", name]);
        command
    };
    command.kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .context("Service status timed out")?
        .context("Failed to query service status")?;
    Ok(output.status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_override_precedes_host_detection() {
        assert!(detect_openwrt(Some("openwrt"), false));
        assert!(!detect_openwrt(Some("systemd"), true));
        assert!(detect_openwrt(None, true));
        assert!(!detect_openwrt(None, false));
    }

    #[test]
    fn service_names_cannot_escape_init_directory() {
        for invalid in [
            "",
            "../network",
            "/etc/init.d/network",
            "x;reboot",
            "-x y",
            "-restart",
            ".",
            "..",
            "network",
            "firewall",
        ] {
            assert!(resolve_openwrt_service_name(invalid, None).is_err());
        }
        assert!(resolve_openwrt_service_name("ModemManager", Some("../other")).is_err());
        assert!(resolve_openwrt_service_name("ModemManager", Some("network")).is_err());
        assert_eq!(
            resolve_openwrt_service_name("ModemManager.service", None).unwrap(),
            "simadmin-modemmanager"
        );
        assert_eq!(
            resolve_openwrt_service_name("ModemManager", Some("modemmanager")).unwrap(),
            "modemmanager"
        );
        assert_eq!(
            resolve_openwrt_service_name("simadmin.service", None).unwrap(),
            "simadmin"
        );
    }
}
