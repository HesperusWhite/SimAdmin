//! Carrier call forwarding through ModemManager's AT channel.
//!
//! Quectel RG200U & Rx500U AT Commands Manual V1.0, section 10.1:
//! https://dl.photonicat.com/pdfs/5G-RM500U/Quectel_RG200URx500U%E7%B3%BB%E5%88%97_AT%E5%91%BD%E4%BB%A4%E6%89%8B%E5%86%8C_V1.0.pdf
//! Only voice class 1 is changed. ModemManager remains the sole serial-port owner.

use async_trait::async_trait;
use simadmin_device_runtime::ModemContext;
use std::time::Duration;
use zbus::Connection;

use crate::models::{CallForwardingResponse, SetCallForwardingRequest};
use crate::{modem_manager::find_modem_path, serial::with_serial};

const AT_TIMEOUT_SECS: u32 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reason {
    Unconditional,
    Busy,
    NoReply,
    NotReachable,
}

impl Reason {
    const ALL: [Self; 4] = [
        Self::Unconditional,
        Self::Busy,
        Self::NoReply,
        Self::NotReachable,
    ];

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "unconditional" | "voice_unconditional" => Ok(Self::Unconditional),
            "busy" | "voice_busy" => Ok(Self::Busy),
            "no_reply" | "voice_no_reply" => Ok(Self::NoReply),
            "not_reachable" | "voice_not_reachable" => Ok(Self::NotReachable),
            _ => Err("forward_type must be unconditional, busy, no_reply, or not_reachable".into()),
        }
    }

    fn code(self) -> u8 {
        match self {
            Self::Unconditional => 0,
            Self::Busy => 1,
            Self::NoReply => 2,
            Self::NotReachable => 3,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct VoiceForwarding {
    enabled: bool,
    number: String,
    timeout: u16,
}

#[derive(Debug)]
struct Update {
    reason: Reason,
    enabled: bool,
    number: String,
    timeout: Option<u16>,
    command: String,
}

impl Update {
    fn new(request: &SetCallForwardingRequest) -> Result<Self, String> {
        let reason = Reason::parse(&request.forward_type)?;
        let enabled = request.enabled.unwrap_or(!request.number.is_empty());
        let number = request.number.as_str();
        if !number.is_empty() {
            let digits = number.strip_prefix('+').unwrap_or(number);
            if digits.is_empty() || digits.len() > 20 || !digits.bytes().all(|c| c.is_ascii_digit())
            {
                return Err("number must contain 1 to 20 digits with an optional leading +".into());
            }
        }
        if enabled && number.is_empty() {
            return Err("number is required when enabling call forwarding".into());
        }
        let timeout = request.timeout.unwrap_or(20);
        if timeout > 30 {
            return Err("timeout must be between 0 and 30 seconds".into());
        }
        let command = if enabled {
            let number_type = if number.starts_with('+') { 145 } else { 129 };
            let mut command = format!("AT+CCFC={},3,\"{number}\",{number_type},1", reason.code());
            if reason == Reason::NoReply {
                command.push_str(&format!(",,,{timeout}"));
            }
            command
        } else {
            // Disabling preserves the registered number and leaves data/fax classes unchanged.
            format!("AT+CCFC={},0,,,1", reason.code())
        };
        Ok(Self {
            reason,
            enabled,
            number: number.to_owned(),
            timeout: (enabled && reason == Reason::NoReply).then_some(timeout),
            command,
        })
    }
}

#[async_trait]
trait AtChannel: Sync {
    async fn command(&self, command: &str) -> Result<String, String>;
}

#[async_trait]
impl AtChannel for ModemContext<'_> {
    async fn command(&self, command: &str) -> Result<String, String> {
        tokio::time::timeout(
            Duration::from_secs(u64::from(AT_TIMEOUT_SECS) + 5),
            self.send_at_command(command, AT_TIMEOUT_SECS),
        )
        .await
        .map_err(|_| "Carrier call forwarding request timed out; query its status before retrying".to_string())?
        .map_err(|error| {
            format!("ModemManager AT request failed: {error}; AT access requires ModemManager started with --debug")
        })
    }
}

fn check_carrier_error(output: &str) -> Result<(), String> {
    if let Some(error) = output.lines().map(str::trim).find(|line| {
        *line == "ERROR" || line.starts_with("+CME ERROR:") || line.starts_with("+CMS ERROR:")
    }) {
        return Err(format!("Carrier rejected call forwarding: {error}"));
    }
    Ok(())
}

fn csv_fields(line: &str) -> Result<Vec<String>, String> {
    let mut fields = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    for character in line.chars() {
        match character {
            '"' => quoted = !quoted,
            ',' if !quoted => {
                fields.push(field.trim().to_owned());
                field.clear();
            }
            _ => field.push(character),
        }
    }
    if quoted {
        return Err("Malformed carrier call forwarding response: unterminated quote".into());
    }
    fields.push(field.trim().to_owned());
    Ok(fields)
}

fn parse_query(output: &str) -> Result<VoiceForwarding, String> {
    check_carrier_error(output)?;
    let mut saw_record = false;
    let mut voice = None;
    for record in output
        .lines()
        .map(str::trim)
        .filter_map(|line| line.strip_prefix("+CCFC:"))
    {
        saw_record = true;
        let fields = csv_fields(record)?;
        let status = fields.first().map(String::as_str).unwrap_or("");
        let enabled = match status {
            "0" => false,
            "1" => true,
            _ => return Err("Malformed carrier call forwarding response: invalid status".into()),
        };
        let class: u16 = fields
            .get(1)
            .and_then(|field| field.parse().ok())
            .ok_or("Malformed carrier call forwarding response: invalid class")?;
        if class & 1 == 0 {
            continue;
        }
        let mut number = fields.get(2).cloned().unwrap_or_default();
        if !number.is_empty()
            && fields.get(3).is_some_and(|value| value == "145")
            && !number.starts_with('+')
        {
            number.insert(0, '+');
        }
        let timeout = match fields.get(6).filter(|value| !value.is_empty()) {
            Some(value) => value
                .parse::<u16>()
                .ok()
                .filter(|value| *value <= 30)
                .ok_or("Malformed carrier call forwarding response: invalid timeout")?,
            None => 0,
        };
        let record = VoiceForwarding {
            enabled,
            number,
            timeout,
        };
        if voice.as_ref().is_some_and(|previous| previous != &record) {
            return Err("Carrier returned conflicting voice call forwarding records".into());
        }
        voice = Some(record);
    }
    if !saw_record {
        return Err("Carrier returned no +CCFC status; call forwarding state is unknown".into());
    }
    Ok(voice.unwrap_or_default())
}

async fn query_all(channel: &impl AtChannel) -> Result<CallForwardingResponse, String> {
    let mut response = CallForwardingResponse::default();
    for reason in Reason::ALL {
        let output = channel
            .command(&format!("AT+CCFC={},2,,,1", reason.code()))
            .await?;
        let voice = parse_query(&output)
            .map_err(|error| format!("Call forwarding reason {}: {error}", reason.code()))?;
        // Preserve legacy fields: an empty number means forwarding is disabled.
        let active_number = if voice.enabled {
            voice.number
        } else {
            String::new()
        };
        match reason {
            Reason::Unconditional => {
                response.voice_unconditional = active_number;
                response.voice_unconditional_enabled = voice.enabled;
            }
            Reason::Busy => {
                response.voice_busy = active_number;
                response.voice_busy_enabled = voice.enabled;
            }
            Reason::NoReply => {
                response.voice_no_reply = active_number;
                response.voice_no_reply_enabled = voice.enabled;
                response.voice_no_reply_timeout = voice.timeout;
            }
            Reason::NotReachable => {
                response.voice_not_reachable = active_number;
                response.voice_not_reachable_enabled = voice.enabled;
            }
        }
    }
    // CCFC queries the carrier; the legacy SIM indicator field is not read or inferred.
    Ok(response)
}

async fn apply_update(
    channel: &impl AtChannel,
    update: Update,
) -> Result<CallForwardingResponse, String> {
    let output = channel.command(&update.command).await?;
    check_carrier_error(&output)?;
    let response = query_all(channel).await.map_err(|error| {
        format!("Carrier accepted the update, but status confirmation failed: {error}. Query status before retrying")
    })?;
    let (enabled, number) = match update.reason {
        Reason::Unconditional => (
            response.voice_unconditional_enabled,
            &response.voice_unconditional,
        ),
        Reason::Busy => (response.voice_busy_enabled, &response.voice_busy),
        Reason::NoReply => (response.voice_no_reply_enabled, &response.voice_no_reply),
        Reason::NotReachable => (
            response.voice_not_reachable_enabled,
            &response.voice_not_reachable,
        ),
    };
    if enabled != update.enabled {
        return Err("Carrier accepted the update but returned a different activation state; query status before retrying".into());
    }
    // Type 145 can add a leading + to the same digits. Do not guess a country code
    // for national numbers: a different returned destination must remain visible.
    if enabled
        && number.strip_prefix('+').unwrap_or(number)
            != update.number.strip_prefix('+').unwrap_or(&update.number)
    {
        return Err("Carrier accepted the update but returned a different forwarding destination; query status before retrying (use an explicit international number if the carrier adds a country code)".into());
    }
    if update
        .timeout
        .is_some_and(|timeout| timeout != response.voice_no_reply_timeout)
    {
        return Err("Carrier accepted the update but returned a different no-reply timeout; query status before retrying".into());
    }
    Ok(response)
}

pub async fn get_call_forwarding(
    connection: &Connection,
) -> Result<CallForwardingResponse, String> {
    let path = find_modem_path(connection)
        .await
        .map_err(|error| error.to_string())?;
    let channel = ModemContext::new(connection, &path).map_err(|error| error.to_string())?;
    with_serial(query_all(&channel)).await
}

pub async fn set_call_forwarding(
    connection: &Connection,
    request: SetCallForwardingRequest,
) -> Result<CallForwardingResponse, String> {
    // Validate before discovering a modem or issuing any AT request.
    let update = Update::new(&request)?;
    let path = find_modem_path(connection)
        .await
        .map_err(|error| error.to_string())?;
    let channel = ModemContext::new(connection, &path).map_err(|error| error.to_string())?;
    with_serial(apply_update(&channel, update)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct MockChannel(Mutex<VecDeque<(&'static str, Result<&'static str, &'static str>)>>);

    #[async_trait]
    impl AtChannel for MockChannel {
        async fn command(&self, command: &str) -> Result<String, String> {
            let (expected, result) = self
                .0
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected AT request");
            assert_eq!(command, expected);
            result.map(str::to_owned).map_err(str::to_owned)
        }
    }

    fn request(
        reason: &str,
        number: &str,
        timeout: Option<u16>,
        enabled: Option<bool>,
    ) -> SetCallForwardingRequest {
        SetCallForwardingRequest {
            forward_type: reason.into(),
            number: number.into(),
            timeout,
            enabled,
        }
    }

    #[test]
    fn registers_voice_number_with_correct_type_and_no_reply_timer() {
        let local = Update::new(&request("busy", "10010", None, None)).unwrap();
        assert_eq!(local.command, "AT+CCFC=1,3,\"10010\",129,1");
        let international =
            Update::new(&request("no_reply", "+8613800000000", Some(25), None)).unwrap();
        assert_eq!(
            international.command,
            "AT+CCFC=2,3,\"+8613800000000\",145,1,,,25"
        );
        assert_eq!(
            Update::new(&request("no_reply", "10010", None, None))
                .unwrap()
                .command,
            "AT+CCFC=2,3,\"10010\",129,1,,,20"
        );
        assert!(Update::new(&request("no_reply", "10010", Some(0), None)).is_ok());
        assert!(Update::new(&request("no_reply", "10010", Some(30), None)).is_ok());
    }

    #[test]
    fn disabling_only_changes_voice_class_and_keeps_legacy_empty_number_semantics() {
        assert_eq!(
            Update::new(&request("unconditional", "", None, None))
                .unwrap()
                .command,
            "AT+CCFC=0,0,,,1"
        );
        assert_eq!(
            Update::new(&request("not_reachable", "10010", None, Some(false)))
                .unwrap()
                .command,
            "AT+CCFC=3,0,,,1"
        );
        let legacy: SetCallForwardingRequest =
            serde_json::from_str(r#"{"forward_type":"unconditional","number":"10010"}"#).unwrap();
        assert!(Update::new(&legacy).unwrap().enabled);
    }

    #[test]
    fn rejects_invalid_requests_and_at_injection() {
        for number in [
            "+",
            "++10010",
            "10010\rAT+CFUN=0",
            "10010\n",
            "10010\"",
            "1,1",
            "10010;",
            "１0010",
            "123456789012345678901",
        ] {
            assert!(
                Update::new(&request("busy", number, None, None)).is_err(),
                "{number:?}"
            );
        }
        assert!(Update::new(&request("all", "10010", None, None)).is_err());
        assert!(Update::new(&request("no_reply", "10010", Some(31), None)).is_err());
        assert!(Update::new(&request("busy", "", None, Some(true))).is_err());
    }

    #[test]
    fn parses_voice_classes_normalized_number_and_timeout() {
        let voice = parse_query("AT+CCFC=2,2,,,1\r\n+CCFC: 1,2,\"123\",129\r\n+CCFC: 1,1,\"8613800000000\",145,,,25\r\nOK\r\n").unwrap();
        assert_eq!(
            voice,
            VoiceForwarding {
                enabled: true,
                number: "+8613800000000".into(),
                timeout: 25
            }
        );
        assert!(
            parse_query("+CCFC: 1,7,\"+8613800000000\",145\nOK")
                .unwrap()
                .enabled
        );
        assert_eq!(
            parse_query("+CCFC: 0,255\nOK").unwrap(),
            VoiceForwarding::default()
        );
        assert_eq!(
            parse_query("+CCFC: 0,1\nOK").unwrap(),
            VoiceForwarding::default()
        );
    }

    #[test]
    fn rejects_carrier_errors_empty_and_malformed_status() {
        for output in [
            "+CME ERROR: 30",
            "ERROR",
            "OK",
            "",
            "+CCFC: 2,1",
            "+CCFC: 1,x",
            "+CCFC: 1,1,\"123",
            "+CCFC: 1,1,\"123\",129,,,31",
            "+CCFC: 1,1,\"123\",129\n+CCFC: 0,1",
        ] {
            assert!(parse_query(output).is_err(), "{output}");
        }
    }

    #[tokio::test]
    async fn reads_all_four_conditions_without_mutation() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 0,1,\"10010\",129")),
            ("AT+CCFC=1,2,,,1", Ok("+CCFC: 1,1,\"10010\",129")),
            ("AT+CCFC=2,2,,,1", Ok("+CCFC: 1,1,\"10010\",129,,,20")),
            ("AT+CCFC=3,2,,,1", Ok("+CCFC: 0,255")),
        ])));
        let response = query_all(&channel).await.unwrap();
        assert!(response.voice_unconditional.is_empty());
        assert!(!response.voice_unconditional_enabled);
        assert!(response.voice_busy_enabled);
        assert_eq!(response.voice_no_reply_timeout, 20);
        assert!(!response.forwarding_flag_on_sim);
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn confirms_successful_no_reply_registration() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=2,3,\"+8613800000000\",145,1,,,25", Ok("OK")),
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=1,2,,,1", Ok("+CCFC: 0,255")),
            (
                "AT+CCFC=2,2,,,1",
                Ok("+CCFC: 1,1,\"8613800000000\",145,,,25"),
            ),
            ("AT+CCFC=3,2,,,1", Ok("+CCFC: 0,255")),
        ])));
        let update = Update::new(&request("no_reply", "+8613800000000", Some(25), None)).unwrap();
        let response = apply_update(&channel, update).await.unwrap();
        assert!(response.voice_no_reply_enabled);
        assert_eq!(response.voice_no_reply, "+8613800000000");
        assert_eq!(response.voice_no_reply_timeout, 25);
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disabled_registered_number_is_not_reported_as_active() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=0,0,,,1", Ok("OK")),
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 0,1,\"10010\",129")),
            ("AT+CCFC=1,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=2,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=3,2,,,1", Ok("+CCFC: 0,255")),
        ])));
        let update = Update::new(&request("unconditional", "", None, None)).unwrap();
        let response = apply_update(&channel, update).await.unwrap();
        assert!(!response.voice_unconditional_enabled);
        assert!(response.voice_unconditional.is_empty());
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn verifies_update_and_reports_partial_success() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=0,3,\"10010\",129,1", Ok("")),
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 1,1,\"10010\",129")),
            ("AT+CCFC=1,2,,,1", Err("network unavailable")),
        ])));
        let update = Update::new(&request("unconditional", "10010", None, None)).unwrap();
        let error = apply_update(&channel, update).await.unwrap_err();
        assert!(error.contains("accepted the update"));
        assert!(error.contains("network unavailable"));
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn carrier_rejection_does_not_trigger_more_commands() {
        let channel = MockChannel(Mutex::new(VecDeque::from([(
            "AT+CCFC=0,0,,,1",
            Ok("+CME ERROR: 3"),
        )])));
        let update = Update::new(&request("unconditional", "", None, None)).unwrap();
        let error = apply_update(&channel, update).await.unwrap_err();
        assert!(error.contains("+CME ERROR: 3"));
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn destination_must_match_the_requested_number() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=0,3,\"+8613800000000\",145,1", Ok("OK")),
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 1,1,\"+8613900000000\",145")),
            ("AT+CCFC=1,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=2,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=3,2,,,1", Ok("+CCFC: 0,255")),
        ])));
        let update = Update::new(&request("unconditional", "+8613800000000", None, None)).unwrap();
        let error = apply_update(&channel, update).await.unwrap_err();
        assert!(error.contains("different forwarding destination"));
        assert!(error.contains("query status before retrying"));
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn no_reply_timeout_must_match_the_requested_timeout() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=2,3,\"+8613800000000\",145,1,,,25", Ok("OK")),
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=1,2,,,1", Ok("+CCFC: 0,255")),
            (
                "AT+CCFC=2,2,,,1",
                Ok("+CCFC: 1,1,\"8613800000000\",145,,,20"),
            ),
            ("AT+CCFC=3,2,,,1", Ok("+CCFC: 0,255")),
        ])));
        let update = Update::new(&request("no_reply", "+8613800000000", Some(25), None)).unwrap();
        let error = apply_update(&channel, update).await.unwrap_err();
        assert!(error.contains("different no-reply timeout"));
        assert!(error.contains("query status before retrying"));
        assert!(channel.0.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn activation_state_must_match_update() {
        let channel = MockChannel(Mutex::new(VecDeque::from([
            ("AT+CCFC=0,3,\"10010\",129,1", Ok("OK")),
            ("AT+CCFC=0,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=1,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=2,2,,,1", Ok("+CCFC: 0,255")),
            ("AT+CCFC=3,2,,,1", Ok("+CCFC: 0,255")),
        ])));
        let update = Update::new(&request("unconditional", "10010", None, None)).unwrap();
        assert!(apply_update(&channel, update)
            .await
            .unwrap_err()
            .contains("different activation state"));
        assert!(channel.0.lock().unwrap().is_empty());
    }
}
