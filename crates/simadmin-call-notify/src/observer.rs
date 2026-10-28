use crate::{parse_calls, Call, Config};
use anyhow::{ensure, Result};
use futures_util::StreamExt;
use std::{collections::HashMap, time::Duration};
use tokio::sync::mpsc;
use zbus::{
    zvariant::{OwnedObjectPath, OwnedValue},
    Connection, MessageStream, Proxy,
};
const SERVICE: &str = "org.freedesktop.ModemManager1";
const MODEM: &str = "org.freedesktop.ModemManager1.Modem";
type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
pub async fn snapshot(conn: &Connection, c: &Config) -> Result<(String, Vec<Call>)> {
    let bus = Proxy::new(
        conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let owner: String = bus.call("GetNameOwner", &(SERVICE,)).await?;
    let manager = Proxy::new(
        conn,
        SERVICE,
        "/org/freedesktop/ModemManager1",
        "org.freedesktop.DBus.ObjectManager",
    )
    .await?;
    let objects: Objects = manager.call("GetManagedObjects", &()).await?;
    let mut matches = Vec::new();
    for (path, interfaces) in objects {
        if !interfaces.contains_key(MODEM) {
            continue;
        };
        let modem = Proxy::new(conn, SERVICE, path.as_str(), MODEM).await?;
        let model: String = modem.get_property("Model").await?;
        let ports: Vec<(String, u32)> = modem.get_property("Ports").await?;
        if model == c.modem_model && ports.iter().any(|(p, _)| p == &c.network_device) {
            matches.push(path)
        }
    }
    ensure!(
        matches.len() == 1,
        "expected exactly one matching modem model and network device"
    );
    let path = &matches[0];
    let modem = Proxy::new(conn, SERVICE, path.as_str(), MODEM).await?;
    // This is the only modem method invoked. Never open a serial device or change settings.
    let output: String = modem.call("Command", &("AT+CLCC", 5u32)).await?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?;
    Ok((
        format!("{}:{}:{}", boot.trim(), owner, path),
        parse_calls(&output)?,
    ))
}
pub async fn watch(c: Config, tx: mpsc::Sender<(String, Vec<Call>)>) -> Result<()> {
    let conn = Connection::system().await?;
    let bus = Proxy::new(
        &conn,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    // Signals wake the query early; periodic reads cover firmware which emits no CallAdded.
    for interface in [
        "org.freedesktop.ModemManager1.Modem.Voice",
        "org.freedesktop.ModemManager1.Call",
    ] {
        let rule = format!("type='signal',sender='{SERVICE}',interface='{interface}'");
        bus.call::<_, _, ()>("AddMatch", &(rule,)).await?;
    }
    let mut signals = MessageStream::from(&conn);
    let mut ticks = tokio::time::interval(Duration::from_millis(c.poll_interval_ms));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut errors = 0u32;
    let mut ready = false;
    loop {
        tokio::select! {_=ticks.tick()=>(),m=signals.next()=>{let Some(m)=m else{anyhow::bail!("D-Bus connection closed")};let m=m?;let h=m.header();let Some(interface)=h.interface()else{continue};if !matches!(interface.as_str(),"org.freedesktop.ModemManager1.Modem.Voice"|"org.freedesktop.ModemManager1.Call"){continue}}}
        match tokio::time::timeout(Duration::from_secs(10), snapshot(&conn, &c)).await {
            Ok(Ok(s)) => {
                if !ready {
                    eprintln!(
                        "Read-only call query ready; target model and usb0 ownership verified"
                    );
                    ready = true;
                }
                if errors > 0 {
                    eprintln!("Read-only call query recovered")
                };
                errors = 0;
                if tx.send(s).await.is_err() {
                    return Ok(());
                }
            }
            _ => {
                errors = errors.saturating_add(1);
                if errors == 1 || errors.is_multiple_of(40) {
                    eprintln!(
                        "Read-only call query unavailable; preserving dedup state and retrying"
                    )
                };
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
