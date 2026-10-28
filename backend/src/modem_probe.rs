//! Explicit one-shot compatibility checks; no database, queue, SMS deletion, or service startup.
use anyhow::Result;
use serde_json::json;
use zbus::{zvariant::OwnedObjectPath, Connection, Proxy};

pub async fn run() -> Result<()> {
    let conn = Connection::system().await?;
    let path = crate::modem_manager::find_modem_path(&conn).await?;
    let network = crate::modem_manager::get_network_info_data(&conn).await?;
    let modem = Proxy::new(
        &conn,
        "org.freedesktop.ModemManager1",
        path.as_str(),
        "org.freedesktop.ModemManager1.Modem",
    )
    .await?;
    let state: i32 = modem.get_property("State").await?;
    let messaging = Proxy::new(
        &conn,
        "org.freedesktop.ModemManager1",
        path.as_str(),
        "org.freedesktop.ModemManager1.Modem.Messaging",
    )
    .await?;
    let storages: Vec<u32> = messaging.get_property("SupportedStorages").await?;
    let messages: Vec<OwnedObjectPath> = messaging.get_property("Messages").await?;
    let forwarding = match crate::call_forwarding::get_call_forwarding(&conn).await {
        Ok(data) => {
            json!({"query_succeeded":true, "unconditional_enabled":data.voice_unconditional_enabled,
            "busy_enabled":data.voice_busy_enabled, "no_reply_enabled":data.voice_no_reply_enabled,
            "not_reachable_enabled":data.voice_not_reachable_enabled})
        }
        Err(error) => json!({"query_succeeded":false,"error":error}),
    };
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "modem_path":path,"modem_enabled":state>=6,"network":network,
            "messaging":{"supported_storages":storages,"message_count":messages.len()},
            "call_forwarding":forwarding,"read_only":true
        }))?
    );
    Ok(())
}
