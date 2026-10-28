//! Read-only runtime diagnostics. This module must never create state or start services.
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

#[derive(Debug, Serialize)]
pub struct Check {
    name: String,
    required: bool,
    passed: bool,
    detail: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    platform: &'static str,
    pub ready: bool,
    checks: Vec<Check>,
}

fn check(name: &str, required: bool, passed: bool, detail: impl Into<String>) -> Check {
    Check {
        name: name.into(),
        required,
        passed,
        detail: detail.into(),
    }
}

fn executable_on_path(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .any(|dir| executable(&dir.join(name)))
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

async fn read_command(program: &str, args: &[&str]) -> Result<String, String> {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        Command::new(program).args(args).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| format!("{program} 查询超时"))?
    .map_err(|err| format!("{program}: {err}"))?;
    if !result.status.success() {
        return Err(format!(
            "{program} 查询失败（{}）: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&result.stdout).trim().to_owned())
}

fn valid_interface(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn service_running(value: &serde_json::Value) -> Result<bool, String> {
    let services = value.as_object().ok_or("服务状态不是 JSON 对象")?;
    let mut running = false;
    for service in services.values() {
        let Some(instances) = service.get("instances") else {
            if !service.is_object() {
                return Err("服务状态不是 JSON 对象".into());
            }
            continue; // procd omits instances for an inactive registered service.
        };
        let instances = instances.as_object().ok_or("服务 instances 格式无效")?;
        for instance in instances.values() {
            let state = instance
                .get("running")
                .and_then(|v| v.as_bool())
                .ok_or("服务实例缺少 running，无法排除串口占用")?;
            running |= state;
        }
    }
    Ok(running)
}

async fn inspect_competing_services() -> Result<(), String> {
    let selected =
        crate::platform::openwrt_service_name("ModemManager").map_err(|err| err.to_string())?;
    for service in [
        "modem",
        "modemmanager",
        "ModemManager",
        "simadmin-modemmanager",
    ] {
        if service == selected {
            continue;
        }
        let filter = serde_json::json!({"name":service}).to_string();
        let output = read_command("ubus", &["call", "service", "list", &filter]).await?;
        let value =
            serde_json::from_str(&output).map_err(|err| format!("服务状态解析失败: {err}"))?;
        if service_running(&value)? {
            return Err(format!("竞争服务 {service} 正在运行；诊断不会自动停止它"));
        }
    }
    Ok(())
}

async fn inspect_modem_bus() -> anyhow::Result<String> {
    let connection = zbus::Connection::system().await?;
    let proxy = zbus::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let owned: bool = proxy
        .call("NameHasOwner", &("org.freedesktop.ModemManager1",))
        .await?;
    anyhow::ensure!(owned, "系统总线上没有 ModemManager，先准备运行依赖");
    let objects = zbus::fdo::ObjectManagerProxy::builder(&connection)
        .destination("org.freedesktop.ModemManager1")?
        .path("/org/freedesktop/ModemManager1")?
        .build()
        .await?
        .get_managed_objects()
        .await?;
    let count = objects
        .values()
        .filter(|interfaces| {
            interfaces
                .keys()
                .any(|name| name.as_str() == "org.freedesktop.ModemManager1.Modem")
        })
        .count();
    anyhow::ensure!(count > 0, "ModemManager 已运行，但尚未发现调制解调器");
    if crate::platform::is_openwrt() {
        let path = crate::modem_manager::find_modem_path(&connection).await?;
        let modem = zbus::Proxy::new(
            &connection,
            "org.freedesktop.ModemManager1",
            path.as_str(),
            "org.freedesktop.ModemManager1.Modem",
        )
        .await?;
        let state: i32 = modem.get_property("State").await?;
        anyhow::ensure!(state >= 6, "RG200U 已发现但未启用，短信接口尚不可用");
        let messaging = zbus::Proxy::new(
            &connection,
            "org.freedesktop.ModemManager1",
            path.as_str(),
            "org.freedesktop.ModemManager1.Modem.Messaging",
        )
        .await?;
        let storages: Vec<u32> = messaging.get_property("SupportedStorages").await?;
        anyhow::ensure!(!storages.is_empty(), "RG200U 没有可用的短信存储能力");
        return Ok(format!("已只读确认 RG200U 拥有 usb0: {path}；未发送 AT"));
    }
    Ok(format!(
        "发现 {count} 个调制解调器；只读取 D-Bus，不发送 AT"
    ))
}

pub async fn inspect() -> Report {
    let openwrt = crate::platform::is_openwrt();
    let mut checks = Vec::new();
    for program in if openwrt {
        vec![
            "ModemManager",
            "mmcli",
            "ubus",
            "uci",
            "ip",
            "dbus-send",
            "jsonfilter",
            "pidof",
        ]
    } else {
        vec!["ModemManager", "mmcli", "nmcli", "systemctl"]
    } {
        checks.push(check(
            program,
            true,
            executable_on_path(program),
            format!("PATH 中的 {program}"),
        ));
    }
    match tokio::time::timeout(Duration::from_secs(5), inspect_modem_bus()).await {
        Ok(Ok(detail)) => checks.push(check("modem_dbus", true, true, detail)),
        Ok(Err(err)) => checks.push(check("modem_dbus", true, false, err.to_string())),
        Err(_) => checks.push(check("modem_dbus", true, false, "D-Bus 查询超时")),
    }
    if openwrt {
        let network = std::env::var("SIMADMIN_NETWORK").unwrap_or_else(|_| "wwan_5g_0".into());
        let interface = std::env::var("SIMADMIN_INTERFACE").unwrap_or_else(|_| "usb0".into());
        let valid = valid_interface(&network) && interface == "usb0";
        checks.push(check(
            "network_names",
            true,
            valid,
            format!("network={network}, device={interface}"),
        ));
        if valid {
            let proto =
                read_command("uci", &["-q", "get", &format!("network.{network}.proto")]).await;
            let device =
                read_command("uci", &["-q", "get", &format!("network.{network}.device")]).await;
            let matches =
                proto.as_deref() == Ok("dhcp") && device.as_deref() == Ok(interface.as_str());
            checks.push(check(
                "netifd_config",
                true,
                matches,
                format!("需要 proto=dhcp, device={interface}；proto={proto:?}, device={device:?}"),
            ));
            let status = read_command(
                "ubus",
                &[
                    "call",
                    &format!("network.interface.{network}"),
                    "status",
                    "{}",
                ],
            )
            .await;
            let result = status.and_then(|text| {
                serde_json::from_str::<serde_json::Value>(&text).map_err(|err| err.to_string())
            });
            checks.push(match result {
                Ok(value) => check(
                    "netifd",
                    true,
                    value.is_object(),
                    format!("netifd 可查询，当前 up={}（未执行 up/down）", value["up"]),
                ),
                Err(err) => check("netifd", true, false, err),
            });
            checks.push(check(
                "network_device",
                true,
                PathBuf::from("/sys/class/net").join(&interface).exists(),
                interface,
            ));
        }
        let configuration = match crate::kwrt_network::KwrtNetworkConfig::from_env() {
            Ok(config) => crate::kwrt_network::inspect_configuration(&config).await,
            Err(err) => Err(err),
        };
        checks.push(match configuration {
            Ok(()) => check(
                "cellular_safety_config",
                true,
                true,
                "蜂窝路由优先级、DNS 与 IPv6 关联配置通过",
            ),
            Err(err) => check("cellular_safety_config", true, false, err),
        });
        checks.push(match inspect_competing_services().await {
            Ok(()) => check("modem_service_ownership", true, true, "未发现竞争拨号服务"),
            Err(err) => check("modem_service_ownership", true, false, err),
        });
        checks.push(check(
            "procd_assets",
            false,
            Path::new("/etc/init.d/simadmin").exists(),
            "部署时才需要 /etc/init.d/simadmin；doctor 不安装它",
        ));
    }
    let ready = checks.iter().all(|item| !item.required || item.passed);
    Report {
        platform: if openwrt { "openwrt" } else { "systemd" },
        ready,
        checks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_config_names_before_uci_or_ubus_queries() {
        for value in ["wwan_5g_0", "usb0", "wan"] {
            assert!(valid_interface(value));
        }
        for value in ["", "../wan", "wan.proto", "wan;reboot", "usb0\n", "@wan"] {
            assert!(!valid_interface(value));
        }
    }

    #[test]
    fn rejects_uncertain_service_state() {
        assert!(!service_running(&serde_json::json!({})).unwrap());
        assert!(!service_running(
            &serde_json::json!({"modem":{"instances":{"one":{"running":false}}}})
        )
        .unwrap());
        assert!(service_running(
            &serde_json::json!({"modem":{"instances":{"one":{"running":true}}}})
        )
        .unwrap());
        for value in [
            serde_json::json!([]),
            serde_json::json!({"modem":{"instances":[]}}),
            serde_json::json!({"modem":{"instances":{"one":{}}}}),
        ] {
            assert!(service_running(&value).is_err());
        }
    }

    #[test]
    fn finds_only_executable_regular_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tool");
        assert!(!executable(&path));
        std::fs::write(&path, b"test").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!executable(&path));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(executable(&path));
        assert!(!executable(dir.path()));
    }
}
