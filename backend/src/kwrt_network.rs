//! RG200U CID 1 is controlled through ModemManager; netifd owns usb0 IP settings.
//! This adapter never changes IMS or any wired interface.
use crate::{config::ApnConfig, models::DataPathHealth};
use async_trait::async_trait;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KwrtNetworkConfig {
    pub network: String,
    pub interface: String,
    pub ipv6_network: String,
}
impl KwrtNetworkConfig {
    pub fn from_env() -> Result<Self, String> {
        Self::new(
            std::env::var("SIMADMIN_NETWORK").unwrap_or_else(|_| "wwan_5g_0".into()),
            std::env::var("SIMADMIN_INTERFACE").unwrap_or_else(|_| "usb0".into()),
            std::env::var("SIMADMIN_IPV6_NETWORK").unwrap_or_else(|_| "wwan6_5g_0".into()),
        )
    }
    fn new(network: String, interface: String, ipv6_network: String) -> Result<Self, String> {
        for name in [&network, &ipv6_network] {
            if name.is_empty()
                || name.len() > 64
                || !name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
            {
                return Err("Invalid netifd interface name".into());
            }
        }
        if network == ipv6_network {
            return Err("IPv4 and IPv6 netifd profiles must differ".into());
        }
        if interface != "usb0" {
            return Err(
                "This RG200U adapter is restricted to usb0; wired interfaces are forbidden".into(),
            );
        }
        Ok(Self {
            network,
            interface,
            ipv6_network,
        })
    }
    pub fn object(&self) -> String {
        format!("network.interface.{}", self.network)
    }
}

#[async_trait]
pub trait NetworkTransport: Sync {
    async fn at(&self, command: &str) -> Result<String, String>;
    async fn netifd(&self, object: &str, method: &str) -> Result<Value, String>;
    async fn profile_config(&self, network: &str) -> Result<Value, String>;
    async fn home_plmn(&self) -> Result<String, String>;
    async fn wait(&self) {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

pub fn validate_profile(status: &Value, config: &KwrtNetworkConfig) -> Result<(), String> {
    if status["proto"].as_str() != Some("dhcp") {
        return Err(format!("netifd {} must use proto=dhcp; ModemManager/QMI/MBIM profiles would create a second data owner", config.network));
    }
    if status["device"].as_str() != Some(config.interface.as_str()) {
        return Err(format!(
            "netifd {} must use device={}; wired interfaces are forbidden",
            config.network, config.interface
        ));
    }
    if let Some(device) = status["l3_device"].as_str() {
        if device != config.interface {
            return Err(format!("Unexpected netifd L3 device {device}"));
        }
    }
    Ok(())
}
fn validate_ipv6(status: &Value, config: &KwrtNetworkConfig) -> Result<(), String> {
    let reference = format!("@{}", config.network);
    if status["proto"].as_str() != Some("dhcpv6")
        || status["device"].as_str() != Some(reference.as_str())
    {
        return Err(format!(
            "netifd {} must be a DHCPv6 alias bound to {}; refusing to touch another interface",
            config.ipv6_network, reference
        ));
    }
    if let Some(device) = status["l3_device"].as_str() {
        if device != config.interface {
            return Err(format!("Unexpected IPv6 L3 device {device}"));
        }
    }
    Ok(())
}

/// IMS CID 11 never counts as normal data.
pub fn data_pdp_active(output: &str) -> Result<bool, String> {
    for line in output.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("+CGACT:") {
            let fields = value.split(',').map(str::trim).collect::<Vec<_>>();
            if fields.len() == 2 && fields[0] == "1" {
                return match fields[1] {
                    "1" => Ok(true),
                    "0" => Ok(false),
                    _ => Err("Invalid CID 1 activity state".into()),
                };
            }
        }
    }
    Err("CID 1 absent from CGACT response; data PDP status unknown".into())
}
fn usable_address(status: &Value, key: &str) -> bool {
    status[key].as_array().is_some_and(|addresses| {
        addresses.iter().any(|entry| {
            entry["address"]
                .as_str()
                .and_then(|value| value.parse::<std::net::IpAddr>().ok())
                .is_some_and(|address| match address {
                    std::net::IpAddr::V4(v) => {
                        !v.is_unspecified()
                            && !v.is_loopback()
                            && !v.is_link_local()
                            && !v.is_multicast()
                    }
                    std::net::IpAddr::V6(v) => {
                        !v.is_unspecified()
                            && !v.is_loopback()
                            && !v.is_unicast_link_local()
                            && !v.is_multicast()
                    }
                })
        })
    })
}
pub fn health_from_status(
    status: &Value,
    config: &KwrtNetworkConfig,
    pdp_active: bool,
) -> DataPathHealth {
    if let Err(error) = validate_profile(status, config) {
        return DataPathHealth {
            reason: Some(error),
            ..DataPathHealth::default()
        };
    }
    let profile_active = status["up"].as_bool().unwrap_or(false);
    let has_address =
        usable_address(status, "ipv4-address") || usable_address(status, "ipv6-address");
    let has_default_route = status["route"].as_array().is_some_and(|routes| {
        routes.iter().any(|route| {
            matches!(route["target"].as_str(), Some("0.0.0.0" | "::"))
                && route["mask"].as_u64() == Some(0)
        })
    });
    let reason = if !pdp_active {
        Some("Normal-data PDP CID 1 is inactive".into())
    } else if !profile_active {
        Some(format!("netifd {} is down", config.network))
    } else if !has_address {
        Some("usb0 has no usable IP address".into())
    } else if !has_default_route {
        Some("netifd cellular profile has no default route".into())
    } else {
        None
    };
    DataPathHealth {
        control_plane_connected: pdp_active,
        profile_active,
        interface: Some(config.interface.clone()),
        has_address,
        has_default_route,
        data_plane_ready: reason.is_none(),
        reason,
    }
}
pub async fn health(
    transport: &impl NetworkTransport,
    config: &KwrtNetworkConfig,
) -> Result<DataPathHealth, String> {
    let status = transport.netifd(&config.object(), "status").await?;
    validate_profile(&status, config)?;
    let active = data_pdp_active(&transport.at("AT+CGACT?").await?)?;
    Ok(health_from_status(&status, config, active))
}
pub fn registration_status(output: &str, prefix: &str) -> Option<u8> {
    output.lines().map(str::trim).find_map(|line| {
        let fields = line.strip_prefix(prefix)?.split(',').collect::<Vec<_>>();
        if fields.len() < 2 {
            return None;
        }
        fields[1].trim().parse().ok()
    })
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RegistrationFamily {
    FiveG,
    Lte,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServingNetwork {
    pub operator_name: String,
    pub plmn: String,
    /// Uses the existing web API labels: "nr" or "lte".
    pub technology: String,
    pub roaming: bool,
    registration_family: RegistrationFamily,
}

fn valid_plmn(value: &str) -> bool {
    matches!(value.len(), 5 | 6) && value.bytes().all(|value| value.is_ascii_digit())
}

fn response_fields(output: &str, prefix: &str) -> Result<Vec<String>, String> {
    let line = output
        .lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix(prefix))
        .ok_or_else(|| format!("Missing {prefix} response; serving network is unknown"))?;
    let mut quoted = false;
    let mut start = 0;
    let mut fields = Vec::new();
    for (index, value) in line.char_indices() {
        match value {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(&line[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if quoted {
        return Err(format!("Unbalanced quotes in {prefix} response"));
    }
    fields.push(&line[start..]);
    fields
        .into_iter()
        .map(|field| {
            let field = field.trim();
            let value = if field.starts_with('"') && field.ends_with('"') && field.len() >= 2 {
                &field[1..field.len() - 1]
            } else {
                field
            };
            if value.contains('"') || value.chars().any(char::is_control) {
                return Err(format!("Malformed field in {prefix} response"));
            }
            Ok(value.to_owned())
        })
        .collect()
}

fn parse_serving_network(
    qnwinfo: &str,
    cops: &str,
    home_plmn: &str,
) -> Result<ServingNetwork, String> {
    let qnwinfo = response_fields(qnwinfo, "+QNWINFO:")?;
    if qnwinfo.len() != 4
        || !valid_plmn(&qnwinfo[1])
        || qnwinfo[2].is_empty()
        || qnwinfo[3]
            .parse::<u32>()
            .ok()
            .filter(|channel| *channel > 0)
            .is_none()
    {
        return Err("QNWINFO does not confirm a serving 5G/LTE network".into());
    }
    let (technology, registration_family, expected_act) = match qnwinfo[0].as_str() {
        "NR5G-SA" => ("nr", RegistrationFamily::FiveG, 11),
        // NSA uses an E-UTRAN anchor, so LTE registration is authoritative.
        "NR5G-NSA" => ("nr", RegistrationFamily::Lte, 7),
        "FDD LTE" | "TDD LTE" | "LTE" => ("lte", RegistrationFamily::Lte, 7),
        _ => return Err("QNWINFO reports no supported serving 5G/LTE network".into()),
    };
    let cops = response_fields(cops, "+COPS:")?;
    if cops.len() != 4
        || !matches!(cops[0].as_str(), "0" | "1" | "4")
        || cops[2].is_empty()
        || cops[3].parse::<u8>().ok() != Some(expected_act)
    {
        return Err("COPS does not confirm the QNWINFO serving network".into());
    }
    match cops[1].as_str() {
        "0" | "1" => {}
        "2" if valid_plmn(&cops[2]) && cops[2] == qnwinfo[1] => {}
        _ => return Err("COPS and QNWINFO PLMN information is invalid or contradictory".into()),
    }
    let home_plmn = home_plmn.trim();
    if !valid_plmn(home_plmn) {
        return Err("SIM home PLMN is unavailable; roaming status is unknown".into());
    }
    Ok(ServingNetwork {
        operator_name: cops[2].clone(),
        plmn: qnwinfo[1].clone(),
        technology: technology.into(),
        roaming: qnwinfo[1] != home_plmn,
        registration_family,
    })
}

/// RG200U fallback when ModemManager consumes C5GREG/CEREG query responses as URCs.
/// QNWINFO and COPS must agree before comparing the serving PLMN with the SIM home PLMN.
/// This function performs read-only operations and relies on its caller's serial lock.
pub async fn serving_network(transport: &impl NetworkTransport) -> Result<ServingNetwork, String> {
    let qnwinfo = transport.at("AT+QNWINFO").await?;
    let cops = transport.at("AT+COPS?").await?;
    let home_plmn = transport.home_plmn().await?;
    parse_serving_network(&qnwinfo, &cops, &home_plmn)
}

pub async fn registered_roaming(transport: &impl NetworkTransport) -> Result<bool, String> {
    let mut five_g = None;
    let mut lte = None;
    for (command, prefix, slot) in [
        ("AT+C5GREG?", "+C5GREG:", &mut five_g),
        ("AT+CEREG?", "+CEREG:", &mut lte),
    ] {
        if let Ok(output) = transport.at(command).await {
            *slot = registration_status(&output, prefix);
            match *slot {
                Some(1) => return Ok(false),
                Some(5) => return Ok(true),
                _ => {}
            }
        }
    }
    let serving = serving_network(transport).await?;
    let relevant_status = match serving.registration_family {
        RegistrationFamily::FiveG => five_g,
        RegistrationFamily::Lte => lte,
    };
    // An explicit result for the active RAT must not be overridden by stale vendor data.
    if relevant_status.is_some() {
        return Err("Registration status contradicts the QNWINFO/COPS serving network".into());
    }
    Ok(serving.roaming)
}
fn pdp_definition(apn: &ApnConfig) -> Result<String, String> {
    let fallback = std::env::var("SIMADMIN_APN").unwrap_or_else(|_| "3gnet".into());
    let name = if apn.apn.trim().is_empty() {
        fallback.trim()
    } else {
        apn.apn.trim()
    };
    if name.is_empty()
        || name.len() > 100
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
    {
        return Err("APN contains unsupported characters".into());
    }
    if !apn.username.is_empty() || !apn.password.is_empty() {
        return Err("RG200U adapter supports APNs without username/password only".into());
    }
    let protocol = match apn.protocol.to_ascii_lowercase().as_str() {
        "dual" | "ipv4v6" => "IPV4V6",
        "ipv4" | "ip" => "IP",
        "ipv6" => return Err("The DHCP IPv4 profile cannot use an IPv6-only APN".into()),
        _ => return Err("Unsupported APN protocol".into()),
    };
    Ok(format!("AT+CGDCONT=1,\"{protocol}\",\"{name}\""))
}
async fn verify_modem(transport: &impl NetworkTransport) -> Result<(), String> {
    if !transport
        .at("ATI")
        .await?
        .to_ascii_uppercase()
        .contains("RG200U")
    {
        return Err(
            "This OpenWrt data adapter requires Quectel RG200U; no PDP changes made".into(),
        );
    }
    Ok(())
}
pub async fn set_active(
    transport: &impl NetworkTransport,
    config: &KwrtNetworkConfig,
    active: bool,
    allow_roaming: bool,
    apn: &ApnConfig,
) -> Result<(), String> {
    let definition = if active {
        Some(pdp_definition(apn)?)
    } else {
        None
    };
    let initial = transport.netifd(&config.object(), "status").await?;
    validate_profile(&initial, config)?;
    let primary = transport.profile_config(&config.network).await?;
    let ipv6 = transport.profile_config(&config.ipv6_network).await?;
    if active {
        validate_configuration(&primary, &ipv6, config)?;
    } else {
        validate_profile(&primary, config)?;
        validate_ipv6(&ipv6, config)?;
    }
    verify_modem(transport).await?;
    if !active {
        let v4_result = transport.netifd(&config.object(), "down").await;
        let modem_result = transport.at("AT+QNETDEVCTL=1,0,0").await;
        v4_result?;
        modem_result?;
        return Ok(());
    }
    if registered_roaming(transport).await? && !allow_roaming {
        return Err("Data activation refused: roaming is disabled".into());
    }
    let pdp_active = data_pdp_active(&transport.at("AT+CGACT?").await?)?;
    if health_from_status(&initial, config, pdp_active).data_plane_ready {
        return Ok(());
    }
    let activation = async {
        if pdp_active {
            transport.at("AT+QNETDEVCTL=1,0,0").await?;
        }
        transport
            .at(definition.as_deref().expect("APN already validated"))
            .await?;
        transport.at("AT+QNETDEVCTL=1,1,1").await?;
        transport.netifd(&config.object(), "up").await?;
        let mut reason = String::from("No usable address or default route");
        for attempt in 0..30 {
            let result = health(transport, config).await?;
            if result.data_plane_ready {
                return Ok(());
            }
            reason = result.reason.unwrap_or(reason);
            if attempt + 1 < 30 {
                transport.wait().await;
            }
        }
        Err(format!(
            "netifd data connection did not become ready: {reason}"
        ))
    }
    .await;
    if let Err(error) = activation {
        let mut cleanup_errors = Vec::new();
        if let Err(cleanup) = transport.netifd(&config.object(), "down").await {
            cleanup_errors.push(cleanup);
        }
        if let Err(cleanup) = transport.at("AT+QNETDEVCTL=1,0,0").await {
            cleanup_errors.push(cleanup);
        }
        return if cleanup_errors.is_empty() {
            Err(error)
        } else {
            Err(format!(
                "{error}; rollback failed, CID 1 state unknown: {}",
                cleanup_errors.join("; ")
            ))
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;
    fn config() -> KwrtNetworkConfig {
        KwrtNetworkConfig::new("wwan_5g_0".into(), "usb0".into(), "wwan6_5g_0".into()).unwrap()
    }
    fn down() -> Value {
        json!({"proto":"dhcp","device":"usb0","up":false})
    }
    fn up() -> Value {
        json!({"proto":"dhcp","device":"usb0","l3_device":"usb0","up":true,"ipv4-address":[{"address":"192.0.2.28","mask":24}],"route":[{"target":"0.0.0.0","mask":0,"nexthop":"192.0.2.1"}]})
    }
    struct Fake {
        calls: Mutex<Vec<String>>,
        running: Mutex<bool>,
        registration: u8,
        registration_consumed: bool,
        home_plmn: String,
        fail_up: bool,
        fail_disconnect: bool,
        status: Value,
        ipv6_device: String,
        primary: Value,
    }
    impl Default for Fake {
        fn default() -> Self {
            Self {
                calls: Mutex::new(vec![]),
                running: Mutex::new(false),
                registration: 1,
                registration_consumed: false,
                home_plmn: "46001".into(),
                fail_up: false,
                fail_disconnect: false,
                status: down(),
                ipv6_device: "@wwan_5g_0".into(),
                primary: json!({"proto":"dhcp","device":"usb0","metric":"600","peerdns":"0","delegate":"0"}),
            }
        }
    }
    #[async_trait]
    impl NetworkTransport for Fake {
        async fn at(&self, command: &str) -> Result<String, String> {
            self.calls.lock().unwrap().push(command.into());
            Ok(match command {
                "ATI" => "Quectel\r\nRG200U-CN\r\nOK".into(),
                "AT+C5GREG?" if self.registration_consumed => String::new(),
                "AT+CEREG?" if self.registration_consumed => String::new(),
                "AT+C5GREG?" => format!("+C5GREG: 0,{}", self.registration),
                "AT+CEREG?" => "+CEREG: 0,0".into(),
                "AT+QNWINFO" => "+QNWINFO: \"NR5G-SA\",46001,\"NR N78\",627264".into(),
                "AT+COPS?" => "+COPS: 0,0,\"CHN-UNICOM\",11".into(),
                "AT+CGACT?" => format!(
                    "+CGACT: 1,{}\r\n+CGACT: 11,1",
                    u8::from(*self.running.lock().unwrap())
                ),
                "AT+QNETDEVCTL=1,1,1" => {
                    *self.running.lock().unwrap() = true;
                    "OK".into()
                }
                "AT+QNETDEVCTL=1,0,0" => {
                    if self.fail_disconnect {
                        return Err("disconnect rejected".into());
                    }
                    *self.running.lock().unwrap() = false;
                    "OK".into()
                }
                value if value.starts_with("AT+CGDCONT=1,") => "OK".into(),
                _ => panic!("Unexpected AT command {command}"),
            })
        }
        async fn netifd(&self, object: &str, method: &str) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("{object}:{method}"));
            assert!(matches!(
                object,
                "network.interface.wwan_5g_0" | "network.interface.wwan6_5g_0"
            ));
            if method == "up" && self.fail_up {
                return Err("DHCP failure".into());
            }
            if object == "network.interface.wwan6_5g_0" {
                return Ok(json!({"proto":"dhcpv6","device":self.ipv6_device,"up":false}));
            }
            Ok(if method == "status" && *self.running.lock().unwrap() {
                up()
            } else {
                self.status.clone()
            })
        }
        async fn profile_config(&self, network: &str) -> Result<Value, String> {
            self.calls.lock().unwrap().push(format!("uci:{network}"));
            if network == "wwan_5g_0" {
                return Ok(self.primary.clone());
            }
            assert_eq!(network, "wwan6_5g_0");
            Ok(
                json!({"proto":"dhcpv6","device":self.ipv6_device,"defaultroute":"0","peerdns":"0","delegate":"0"}),
            )
        }
        async fn home_plmn(&self) -> Result<String, String> {
            self.calls
                .lock()
                .unwrap()
                .push("dbus:Sim.OperatorIdentifier".into());
            Ok(self.home_plmn.clone())
        }
        async fn wait(&self) {}
    }
    #[tokio::test]
    async fn consumed_registration_queries_use_observed_rg200u_sa_evidence() {
        let fake = Fake {
            registration_consumed: true,
            ..Fake::default()
        };
        let serving = serving_network(&fake).await.unwrap();
        assert_eq!(serving.operator_name, "CHN-UNICOM");
        assert_eq!(serving.plmn, "46001");
        assert_eq!(serving.technology, "nr");
        assert!(!serving.roaming);
        assert!(!registered_roaming(&fake).await.unwrap());
        assert!(!fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.contains("=") || command.ends_with(":up")));
    }

    #[tokio::test]
    async fn consumed_queries_allow_home_sa_activation_without_touching_ims() {
        let fake = Fake {
            registration_consumed: true,
            ..Fake::default()
        };
        set_active(&fake, &config(), true, false, &ApnConfig::default())
            .await
            .unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(calls.contains(&"dbus:Sim.OperatorIdentifier".into()));
        assert!(calls.contains(&"AT+QNETDEVCTL=1,1,1".into()));
        assert!(!calls.iter().any(|command| command.contains("=11,")));
    }

    #[tokio::test]
    async fn fallback_foreign_plmn_is_roaming_and_cannot_bypass_policy() {
        let fake = Fake {
            registration_consumed: true,
            home_plmn: "310260".into(),
            ..Fake::default()
        };
        assert!(registered_roaming(&fake).await.unwrap());
        let error = set_active(&fake, &config(), true, false, &ApnConfig::default())
            .await
            .unwrap_err();
        assert!(error.contains("roaming is disabled"));
        assert!(!fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.starts_with("AT+QNETDEVCTL=")
                || command.starts_with("AT+CGDCONT=")
                || command.ends_with(":up")));
    }

    #[tokio::test]
    async fn unavailable_sim_home_plmn_is_unknown_and_never_treated_as_home() {
        let fake = Fake {
            registration_consumed: true,
            home_plmn: String::new(),
            ..Fake::default()
        };
        let error = registered_roaming(&fake).await.unwrap_err();
        assert!(error.contains("roaming status is unknown"));
        assert!(
            set_active(&fake, &config(), true, false, &ApnConfig::default())
                .await
                .is_err()
        );
        assert!(!fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|command| command.starts_with("AT+QNETDEVCTL=") || command.ends_with(":up")));
    }

    #[tokio::test]
    async fn standard_registration_remains_authoritative_without_home_property() {
        for (registration, roaming) in [(1, false), (5, true)] {
            let fake = Fake {
                registration,
                home_plmn: String::new(),
                ..Fake::default()
            };
            assert_eq!(registered_roaming(&fake).await.unwrap(), roaming);
            let calls = fake.calls.lock().unwrap();
            assert!(!calls.iter().any(|command| matches!(
                command.as_str(),
                "AT+QNWINFO" | "AT+COPS?" | "dbus:Sim.OperatorIdentifier"
            )));
        }
    }

    #[tokio::test]
    async fn explicit_unregistered_status_cannot_be_overridden_by_stale_vendor_data() {
        let fake = Fake {
            registration: 0,
            ..Fake::default()
        };
        assert!(registered_roaming(&fake)
            .await
            .unwrap_err()
            .contains("contradicts"));
    }

    #[test]
    fn serving_network_rejects_missing_contradictory_or_malformed_evidence() {
        let qnwinfo = "+QNWINFO: \"NR5G-SA\",46001,\"NR N78\",627264";
        let cops = "+COPS: 0,0,\"CHN-UNICOM\",11";
        for bad_cops in [
            "",
            "+COPS: 2",
            "+COPS: 2,0,\"CHN-UNICOM\",11",
            "+COPS: 0,0,\"\",11",
            "+COPS: 0,0,\"CHN-UNICOM\",7",
            "+COPS: 0,2,\"46006\",11",
            "+COPS: 0,2,\"CHN-UNICOM\",11",
            "+COPS: 0,0,\"CHN-UNICOM,11",
        ] {
            assert!(
                parse_serving_network(qnwinfo, bad_cops, "46001").is_err(),
                "accepted {bad_cops}"
            );
        }
        for bad_qnwinfo in [
            "",
            "+QNWINFO: No Service",
            "+QNWINFO: \"GSM\",46001,\"GSM 900\",10",
            "+QNWINFO: \"NR5G-SA\",4600,\"NR N78\",627264",
            "+QNWINFO: \"NR5G-SA\",46001,\"NR N78\",0",
        ] {
            assert!(
                parse_serving_network(bad_qnwinfo, cops, "46001").is_err(),
                "accepted {bad_qnwinfo}"
            );
        }
        for home in ["", "unknown", "4600", "4600011", "4600x"] {
            assert!(parse_serving_network(qnwinfo, cops, home).is_err());
        }
    }

    #[test]
    fn serving_network_supports_numeric_cops_and_six_digit_plmn() {
        let qnwinfo = "+QNWINFO: \"FDD LTE\",\"310260\",\"LTE BAND 2\",900";
        let cops = "+COPS: 0,2,\"310260\",7";
        let serving = parse_serving_network(qnwinfo, cops, "310260").unwrap();
        assert_eq!(serving.technology, "lte");
        assert_eq!(serving.plmn, "310260");
        assert!(!serving.roaming);
        assert!(
            parse_serving_network(qnwinfo, cops, "310410")
                .unwrap()
                .roaming
        );
    }

    #[test]
    fn serving_network_parses_a_quoted_operator_name_with_a_comma() {
        let serving = parse_serving_network(
            "+QNWINFO: \"NR5G-SA\",46001,\"NR N78\",627264",
            "+COPS: 0,0,\"Carrier, Name\",11",
            "46001",
        )
        .unwrap();
        assert_eq!(serving.operator_name, "Carrier, Name");
    }

    #[tokio::test]
    async fn missing_route_or_dns_protection_refuses_activation_before_at() {
        for field in ["metric", "peerdns"] {
            let mut fake = Fake::default();
            fake.primary.as_object_mut().unwrap().remove(field);
            assert!(
                set_active(&fake, &config(), true, false, &ApnConfig::default())
                    .await
                    .is_err()
            );
            assert!(!fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("AT") || c.ends_with(":up")));
        }
        let mut fake = Fake::default();
        fake.primary["metric"] = json!("0");
        assert!(
            set_active(&fake, &config(), true, false, &ApnConfig::default())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn unsafe_activation_settings_do_not_prevent_safe_disconnect() {
        let mut fake = Fake::default();
        fake.primary.as_object_mut().unwrap().remove("metric");
        fake.primary["peerdns"] = json!("1");
        set_active(&fake, &config(), false, false, &ApnConfig::default())
            .await
            .unwrap();
        assert!(fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "network.interface.wwan_5g_0:down"));
    }
    #[test]
    fn ipv6_default_route_and_dns_cannot_override_wired_connection() {
        let primary = Fake::default().primary;
        let mut ipv6 = json!({"proto":"dhcpv6","device":"@wwan_5g_0","defaultroute":"0","peerdns":"0","delegate":"0"});
        assert!(validate_configuration(&primary, &ipv6, &config()).is_ok());
        for field in ["defaultroute", "peerdns", "delegate"] {
            ipv6[field] = json!("1");
            assert!(validate_configuration(&primary, &ipv6, &config()).is_err());
            ipv6[field] = json!("0");
        }
    }
    #[test]
    fn rejects_wired_interface_and_object_injection() {
        for name in ["x;reboot", "../wan", "wan.other", "", "wan\n"] {
            assert!(KwrtNetworkConfig::new(name.into(), "usb0".into(), "wwan6".into()).is_err());
        }
        for interface in ["eth0", "eth1", "br-lan", "usb0; reboot"] {
            assert!(
                KwrtNetworkConfig::new("wwan".into(), interface.into(), "wwan6".into()).is_err()
            );
        }
    }
    #[test]
    fn ims_context_never_counts_as_normal_data() {
        assert!(!data_pdp_active("+CGACT: 1,0\r\n+CGACT: 11,1").unwrap());
        assert!(data_pdp_active("+CGACT: 11,1").is_err());
        assert!(data_pdp_active("+CGACT: 1,1\r\n+CGACT: 11,0").unwrap());
    }
    #[test]
    fn health_requires_address_route_and_expected_profile() {
        assert!(health_from_status(&up(), &config(), true).data_plane_ready);
        let mut status = up();
        status["route"] = json!([]);
        assert!(!health_from_status(&status, &config(), true).data_plane_ready);
        status["route"] = up()["route"].clone();
        status["ipv4-address"] = json!([{"address":"169.254.1.1","mask":16}]);
        assert!(!health_from_status(&status, &config(), true).data_plane_ready);
        for device in ["eth0", "eth1"] {
            status = up();
            status["device"] = json!(device);
            assert!(validate_profile(&status, &config()).is_err());
        }
    }
    #[test]
    fn parses_5g_sa_registration() {
        assert_eq!(
            registration_status("+C5GREG: 0,1,\"1234\",\"123456\",11", "+C5GREG:"),
            Some(1)
        );
        assert_eq!(registration_status("+C5GREG: 2,5", "+C5GREG:"), Some(5));
        assert_eq!(registration_status("OK", "+C5GREG:"), None);
    }
    #[tokio::test]
    async fn connects_cid_one_then_netifd_and_keeps_ims() {
        let fake = Fake::default();
        set_active(&fake, &config(), true, false, &ApnConfig::default())
            .await
            .unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(calls.contains(&"AT+CGDCONT=1,\"IPV4V6\",\"3gnet\"".into()));
        assert!(
            calls
                .iter()
                .position(|c| c == "AT+QNETDEVCTL=1,1,1")
                .unwrap()
                < calls
                    .iter()
                    .position(|c| c == "network.interface.wwan_5g_0:up")
                    .unwrap()
        );
        assert!(!calls
            .iter()
            .any(|c| c.starts_with("AT+CGACT=") || c.contains("=11,")));
    }
    #[tokio::test]
    async fn roaming_and_apn_validation_precede_mutation() {
        let fake = Fake {
            registration: 5,
            ..Fake::default()
        };
        assert!(
            set_active(&fake, &config(), true, false, &ApnConfig::default())
                .await
                .is_err()
        );
        assert!(!fake
            .calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.starts_with("AT+QNETDEVCTL=") || c.ends_with(":up")));
        let mut apn = ApnConfig::default();
        apn.apn = "3gnet\"\rAT+CFUN=0".into();
        assert!(set_active(&fake, &config(), true, true, &apn)
            .await
            .is_err());
    }
    #[tokio::test]
    async fn netifd_failure_rolls_back_only_data_cid() {
        let fake = Fake {
            fail_up: true,
            ..Fake::default()
        };
        assert!(
            set_active(&fake, &config(), true, false, &ApnConfig::default())
                .await
                .is_err()
        );
        assert!(!*fake.running.lock().unwrap());
        assert!(fake.calls.lock().unwrap().ends_with(&[
            "network.interface.wwan_5g_0:down".into(),
            "AT+QNETDEVCTL=1,0,0".into()
        ]));
    }
    #[tokio::test]
    async fn reports_disconnect_failure_during_rollback() {
        let fake = Fake {
            fail_up: true,
            fail_disconnect: true,
            ..Fake::default()
        };
        let error = set_active(&fake, &config(), true, false, &ApnConfig::default())
            .await
            .unwrap_err();
        assert!(error.contains("rollback failed, CID 1 state unknown"));
        assert!(*fake.running.lock().unwrap());
    }
    #[tokio::test]
    async fn rejects_second_data_owner_and_wired_ipv6_without_at_commands() {
        for fake in [
            Fake {
                status: json!({"proto":"modemmanager","device":"usb0","up":false}),
                ..Fake::default()
            },
            Fake {
                ipv6_device: "eth1".into(),
                ..Fake::default()
            },
        ] {
            assert!(
                set_active(&fake, &config(), true, false, &ApnConfig::default())
                    .await
                    .is_err()
            );
            assert!(!fake
                .calls
                .lock()
                .unwrap()
                .iter()
                .any(|c| c.starts_with("AT") || c.ends_with(":up") || c.ends_with(":down")));
        }
    }
    #[tokio::test]
    async fn disable_stops_primary_and_its_ipv6_alias_without_registration() {
        let fake = Fake {
            registration: 0,
            ..Fake::default()
        };
        set_active(&fake, &config(), false, false, &ApnConfig::default())
            .await
            .unwrap();
        let calls = fake.calls.lock().unwrap();
        assert!(calls.ends_with(&[
            "network.interface.wwan_5g_0:down".into(),
            "AT+QNETDEVCTL=1,0,0".into()
        ]));
        assert!(!calls.iter().any(|c| c.contains("REG?")));
    }
}

/// Read only the two named cellular UCI sections. Never execute UCI writes or network reloads.
pub async fn read_profile_config(network: &str) -> Result<Value, String> {
    if network.is_empty()
        || !network
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err("Invalid cellular UCI section name".into());
    }
    let mut object = serde_json::Map::new();
    for field in [
        "proto",
        "device",
        "metric",
        "peerdns",
        "defaultroute",
        "delegate",
    ] {
        let key = format!("network.{network}.{field}");
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tokio::process::Command::new("uci")
                .args(["-q", "get", &key])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| format!("UCI read timed out: {key}"))?
        .map_err(|error| format!("Cannot read UCI configuration: {error}"))?;
        if output.status.success() {
            object.insert(
                field.into(),
                Value::String(String::from_utf8_lossy(&output.stdout).trim().into()),
            );
        } else if output.status.code() != Some(1) {
            return Err(format!(
                "Cannot read UCI key {key}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
    }
    Ok(Value::Object(object))
}

pub fn validate_configuration(
    primary: &Value,
    ipv6: &Value,
    config: &KwrtNetworkConfig,
) -> Result<(), String> {
    validate_profile(primary, config)?;
    let metric = primary["metric"]
        .as_str()
        .and_then(|value| value.parse::<u32>().ok());
    if !metric.is_some_and(|value| value >= 600) {
        return Err(format!(
            "network.{}.metric must be >=600 to preserve wired route priority",
            config.network
        ));
    }
    if primary["peerdns"].as_str() != Some("0") {
        return Err(format!(
            "network.{}.peerdns must be 0 to preserve wired DNS",
            config.network
        ));
    }
    if primary["delegate"].as_str() != Some("0") || ipv6["delegate"].as_str() != Some("0") {
        return Err("Both cellular profiles must set delegate=0 to prevent assigning 5G prefixes to wired LAN interfaces".into());
    }
    validate_ipv6(ipv6, config)?;
    if ipv6["defaultroute"].as_str() != Some("0") || ipv6["peerdns"].as_str() != Some("0") {
        return Err(format!(
            "network.{} must set defaultroute=0 and peerdns=0 to preserve wired IPv6 routing/DNS",
            config.ipv6_network
        ));
    }
    Ok(())
}

pub async fn inspect_configuration(config: &KwrtNetworkConfig) -> Result<(), String> {
    let primary = read_profile_config(&config.network).await?;
    let ipv6 = read_profile_config(&config.ipv6_network).await?;
    validate_configuration(&primary, &ipv6, config)
}
