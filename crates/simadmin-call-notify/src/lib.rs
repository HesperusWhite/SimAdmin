//! Read-only incoming-call observer. ModemManager owns the physical ports.
use anyhow::{ensure, Context, Result};
use chrono::{DateTime, FixedOffset, Utc};
use serde::{Deserialize, Serialize};
use simadmin_notify::{ChannelType, NotificationMessage, Sender};
use std::{
    collections::{BTreeMap, VecDeque},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};
use uuid::Uuid;
pub mod observer;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub modem_model: String,
    pub network_device: String,
    pub poll_interval_ms: u64,
    pub data_dir: PathBuf,
    pub secret_file: PathBuf,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: false,
            modem_model: "RG200U-CN".into(),
            network_device: "usb0".into(),
            poll_interval_ms: 1500,
            data_dir: "/opt/simadmin/data/call-notify".into(),
            secret_file: "/etc/codex-telegram-bridge/simadmin-rx.key".into(),
        }
    }
}
impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let c: Self = serde_json::from_slice(&fs::read(path)?)?;
        ensure!(
            !c.modem_model.is_empty() && !c.network_device.is_empty(),
            "explicit modem model and network device required"
        );
        ensure!(
            (500..=10000).contains(&c.poll_interval_ms),
            "poll_interval_ms must be 500..10000"
        );
        ensure!(
            c.data_dir.is_absolute() && c.secret_file.is_absolute(),
            "paths must be absolute"
        );
        ensure!(
            c.data_dir.components().count() > 3
                && !c
                    .data_dir
                    .components()
                    .any(|p| p == std::path::Component::ParentDir),
            "dedicated state directory required"
        );
        Ok(c)
    }
}
pub fn load_secret(path: &Path) -> Result<String> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let m = f.metadata()?;
    ensure!(
        m.is_file() && m.permissions().mode() & 0o077 == 0 && m.len() <= 512,
        "secret must be a private regular file"
    );
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    let s = s.trim();
    ensure!(
        (32..=256).contains(&s.len()),
        "invalid RX signing key length"
    );
    Ok(s.to_owned())
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub index: u32,
    pub number: String,
    pub state: u8,
}
/// Parse only voice calls. An empty successful +CLCC response means no calls.
pub fn parse_calls(text: &str) -> Result<Vec<Call>> {
    ensure!(text.len() <= 65536, "call response too long");
    let mut calls = Vec::new();
    for line in text.lines().map(str::trim).filter(|s| !s.is_empty()) {
        ensure!(
            !line.contains("ERROR"),
            "modem rejected read-only call query"
        );
        let Some(rest) = line.strip_prefix("+CLCC:") else {
            continue;
        };
        let fields: Vec<_> = rest
            .split(',')
            .map(|s| s.trim().trim_matches('"'))
            .collect();
        ensure!(fields.len() >= 5, "invalid CLCC record");
        let index = fields[0].parse::<u32>()?;
        let direction = fields[1].parse::<u8>()?;
        let state = fields[2].parse::<u8>()?;
        let mode = fields[3].parse::<u8>()?;
        ensure!(
            index > 0 && direction <= 1 && state <= 5,
            "invalid call state"
        );
        if direction != 1 || mode != 0 {
            continue;
        }
        let mut number = fields
            .get(5)
            .unwrap_or(&"")
            .chars()
            .filter(|c| !c.is_control())
            .take(64)
            .collect::<String>();
        if fields.get(6) == Some(&"145") && !number.is_empty() && !number.starts_with('+') {
            number.insert(0, '+')
        }
        ensure!(
            !calls.iter().any(|c: &Call| c.index == index),
            "duplicate call index"
        );
        calls.push(Call {
            index,
            number,
            state,
        });
    }
    ensure!(calls.len() <= 64, "too many calls");
    Ok(calls)
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Notification {
    pub id: String,
    pub title: String,
    pub text: String,
}
#[derive(Clone, Serialize, Deserialize)]
struct Seen {
    id: String,
    number: String,
    first_seen: DateTime<Utc>,
    state: u8,
    queued: bool,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct State {
    pub version: u8,
    scope: String,
    active: BTreeMap<u32, Seen>,
    pub pending: VecDeque<Notification>,
}
impl State {
    pub fn reconcile(&mut self, scope: &str, calls: &[Call], now: DateTime<Utc>) -> Result<()> {
        // Work on a copy: a full queue cannot leave partially modified durable state.
        let mut next = self.clone();
        next.observe(scope, calls, now)?;
        *self = next;
        Ok(())
    }
    fn observe(&mut self, scope: &str, calls: &[Call], now: DateTime<Utc>) -> Result<()> {
        let old = std::mem::take(&mut self.active);
        for call in calls {
            let previous = old.get(&call.index).filter(|s| {
                self.scope == scope
                    && (s.number.is_empty() || call.number.is_empty() || s.number == call.number)
            });
            let mut seen = previous.cloned().unwrap_or_else(|| Seen {
                id: format!("kwrt-main:call:{}", Uuid::new_v4()),
                number: String::new(),
                first_seen: now,
                state: call.state,
                queued: false,
            });
            if !call.number.is_empty() {
                seen.number = call.number.clone()
            };
            seen.state = call.state;
            if !seen.queued
                && (!seen.number.is_empty() || (now - seen.first_seen).num_milliseconds() >= 2000)
            {
                self.queue(&seen, false)?;
                seen.queued = true;
            }
            self.active.insert(call.index, seen);
        }
        for (index, seen) in &old {
            if (self.scope != scope || !self.active.contains_key(index)) && !seen.queued {
                self.queue(seen, true)?
            }
        }
        self.scope = scope.to_owned();
        Ok(())
    }
    fn queue(&mut self, s: &Seen, ended: bool) -> Result<()> {
        ensure!(
            self.pending.len() < 1024,
            "call notification queue full; preserving previous state"
        );
        let status = if ended {
            "来电已结束（接听情况未知）"
        } else {
            match s.state {
                4 => "来电响铃",
                5 => "呼叫等待",
                0 => "通话中",
                1 => "通话保持",
                _ => "来电",
            }
        };
        let at = s
            .first_seen
            .with_timezone(&FixedOffset::east_opt(8 * 3600).unwrap())
            .format("%Y-%m-%d %H:%M:%S");
        self.pending.push_back(Notification {
            id: s.id.clone(),
            title: "联通来电".into(),
            text: format!(
                "来电号码：{}\n时间：{}（北京时间）\n状态：{}",
                if s.number.is_empty() {
                    "隐藏或未知号码"
                } else {
                    &s.number
                },
                at,
                status
            ),
        });
        Ok(())
    }
    pub fn enqueue_test(&mut self) -> Result<()> {
        ensure!(self.pending.len() < 1024, "queue full");
        self.pending.push_back(Notification {
            id: format!("kwrt-main:call-test:{}", Uuid::new_v4()),
            title: "来电通知测试".into(),
            text: "来电通知服务已接入当前 Bot。这是一条测试通知，没有实际拨打或接听电话。".into(),
        });
        Ok(())
    }
}
pub struct Store {
    root: PathBuf,
    _lock: File,
    pub state: State,
}
impl Store {
    pub fn open(root: &Path) -> Result<Self> {
        if root.exists() {
            ensure!(
                fs::symlink_metadata(root)?.file_type().is_dir(),
                "state directory must not be a symlink"
            )
        } else {
            fs::create_dir_all(root)?
        };
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join("notify.lock"))?;
        ensure!(
            unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
            "another call observer is running"
        );
        let p = root.join("state.json");
        let state = if p.exists() {
            ensure!(
                fs::symlink_metadata(&p)?.file_type().is_file(),
                "state must be a regular file"
            );
            let s: State = serde_json::from_slice(&fs::read(p)?)
                .context("corrupt call state; refusing to reset")?;
            ensure!(s.version == 1, "unsupported call state version");
            s
        } else {
            State {
                version: 1,
                ..State::default()
            }
        };
        Ok(Self {
            root: root.into(),
            _lock: lock,
            state,
        })
    }
    pub fn save(&self) -> Result<()> {
        let p = self.root.join(format!("state-{}.tmp", Uuid::new_v4()));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&p)?;
        f.write_all(&serde_json::to_vec(&self.state)?)?;
        f.sync_all()?;
        fs::rename(p, self.root.join("state.json"))?;
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}
pub async fn deliver(sender: &Sender, secret: &str, url: &str, n: &Notification) -> Result<()> {
    let config = serde_json::json!({"url":url,"http_method":"POST","headers":{},"secret":secret});
    let r = sender
        .send(
            ChannelType::Webhook,
            &config,
            &NotificationMessage {
                title: n.title.clone(),
                body: String::new(),
                custom_body: Some(serde_json::to_string(n)?),
            },
        )
        .await?;
    let receipt: serde_json::Value = serde_json::from_str(&r.response_summary)?;
    ensure!(
        r.status_code == 202 && receipt.get("accepted").and_then(|v| v.as_bool()) == Some(true),
        "RX did not acknowledge durable acceptance"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn now() -> DateTime<Utc> {
        "2026-10-26T07:00:00Z".parse().unwrap()
    }
    fn incoming(number: &str) -> Call {
        Call {
            index: 1,
            number: number.into(),
            state: 4,
        }
    }
    #[test]
    fn filters_outgoing_and_non_voice() {
        let v=parse_calls("+CLCC: 1,0,2,0,0,\"10010\",129\n+CLCC: 2,1,4,1,0,\"123\",129\n+CLCC: 3,1,4,0,0,\"8613800000000\",145").unwrap();
        assert_eq!(
            v,
            vec![Call {
                index: 3,
                number: "+8613800000000".into(),
                state: 4
            }]
        );
    }
    #[test]
    fn malformed_response_is_not_an_empty_success() {
        for s in [
            "ERROR",
            "+CME ERROR: 3",
            "+CLCC: 1,1",
            "+CLCC: 0,1,4,0,0",
            "+CLCC: 1,1,8,0,0",
        ] {
            assert!(parse_calls(s).is_err(), "{s}")
        }
        assert!(parse_calls("\r\nOK\r\n").unwrap().is_empty());
    }
    #[test]
    fn repeated_ringing_and_answering_notify_once() {
        let mut s = State::default();
        for state in [4, 4, 4, 0, 1] {
            let mut c = incoming("10010");
            c.state = state;
            s.reconcile("modem", &[c], now()).unwrap()
        }
        assert_eq!(s.pending.len(), 1);
        assert!(s.pending[0].text.contains("来电号码：10010"));
        assert!(s.pending[0].text.contains("2026-10-26 15:00:00"));
    }
    #[test]
    fn caller_id_grace_period_avoids_unknown_then_number_duplicates() {
        let mut s = State::default();
        s.reconcile("m", &[incoming("")], now()).unwrap();
        assert!(s.pending.is_empty());
        s.reconcile(
            "m",
            &[incoming("10010")],
            now() + chrono::Duration::seconds(1),
        )
        .unwrap();
        assert_eq!(s.pending.len(), 1);
        assert!(s.pending[0].text.contains("10010"));
    }
    #[test]
    fn hidden_number_and_short_call_are_reported() {
        let mut s = State::default();
        s.reconcile("m", &[incoming("")], now()).unwrap();
        s.reconcile("m", &[], now() + chrono::Duration::seconds(1))
            .unwrap();
        assert_eq!(s.pending.len(), 1);
        assert!(s.pending[0].text.contains("隐藏或未知号码"));
        assert!(s.pending[0].text.contains("已结束"));
    }
    #[test]
    fn hidden_number_is_reported_after_grace() {
        let mut s = State::default();
        s.reconcile("m", &[incoming("")], now()).unwrap();
        s.reconcile("m", &[incoming("")], now() + chrono::Duration::seconds(2))
            .unwrap();
        assert_eq!(s.pending.len(), 1);
    }
    #[test]
    fn reused_call_index_creates_distinct_notifications() {
        let mut s = State::default();
        s.reconcile("m", &[incoming("10010")], now()).unwrap();
        s.reconcile("m", &[], now()).unwrap();
        s.reconcile("m", &[incoming("10010")], now()).unwrap();
        assert_eq!(s.pending.len(), 2);
        assert_ne!(s.pending[0].id, s.pending[1].id);
    }
    #[test]
    fn changed_number_or_modem_owner_is_a_new_call() {
        let mut s = State::default();
        s.reconcile("m", &[incoming("10010")], now()).unwrap();
        s.reconcile("m", &[incoming("10086")], now()).unwrap();
        s.reconcile("new-owner", &[incoming("10086")], now())
            .unwrap();
        assert_eq!(s.pending.len(), 3);
    }
    #[test]
    fn restart_keeps_queue_and_dedup() {
        let d = tempfile::tempdir().unwrap();
        let mut first = Store::open(d.path()).unwrap();
        first
            .state
            .reconcile("m", &[incoming("10010")], now())
            .unwrap();
        first.save().unwrap();
        let id = first.state.pending[0].id.clone();
        drop(first);
        let mut second = Store::open(d.path()).unwrap();
        second
            .state
            .reconcile("m", &[incoming("10010")], now())
            .unwrap();
        assert_eq!(second.state.pending.len(), 1);
        assert_eq!(second.state.pending[0].id, id);
        assert_eq!(
            fs::metadata(d.path().join("state.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    #[test]
    fn corrupt_state_refuses_to_reset_and_singleton_lock_works() {
        let d = tempfile::tempdir().unwrap();
        let first = Store::open(d.path()).unwrap();
        assert!(Store::open(d.path()).is_err());
        drop(first);
        fs::write(d.path().join("state.json"), "invalid").unwrap();
        assert!(Store::open(d.path()).is_err());
    }
    #[test]
    fn queue_full_leaves_observation_unchanged() {
        let mut s = State::default();
        for _ in 0..1024 {
            s.enqueue_test().unwrap()
        }
        let before = serde_json::to_vec(&s).unwrap();
        assert!(s.reconcile("m", &[incoming("10010")], now()).is_err());
        assert_eq!(before, serde_json::to_vec(&s).unwrap());
    }
    #[test]
    fn key_must_be_private_and_not_a_symlink() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("key");
        fs::write(&p, "k".repeat(64)).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_secret(&p).is_ok());
        let link = d.path().join("link");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(load_secret(&link).is_err());
        fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(load_secret(&p).is_err());
    }
    #[tokio::test]
    async fn webhook_signing_json_and_unconfirmed_acknowledgements() {
        use axum::{
            extract::State as AxState,
            http::{HeaderMap, StatusCode},
            routing::post,
            Router,
        };
        use hmac::{Hmac, Mac};
        use sha2::Sha256;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        };
        async fn receive(
            AxState(step): AxState<Arc<AtomicUsize>>,
            headers: HeaderMap,
            body: String,
        ) -> (StatusCode, String) {
            let n: Notification = serde_json::from_str(&body).unwrap();
            assert!(n.id.starts_with("kwrt-main:call:"));
            assert!(n.text.contains("10010"));
            let mut m =
                Hmac::<Sha256>::new_from_slice(b"test-secret-with-at-least-32-bytes").unwrap();
            m.update(body.as_bytes());
            let signature = m
                .finalize()
                .into_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            assert_eq!(headers["x-webhook-signature"], signature);
            match step.fetch_add(1, Ordering::SeqCst) {
                0 => (StatusCode::SERVICE_UNAVAILABLE, "busy".into()),
                1 => (StatusCode::OK, "{}".into()),
                _ => (
                    StatusCode::ACCEPTED,
                    "{\"accepted\":true,\"duplicate\":false,\"queued_messages\":1}".into(),
                ),
            }
        }
        let step = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/rx/simadmin/json", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/rx/simadmin/json", post(receive))
                    .with_state(step),
            )
            .await
            .unwrap()
        });
        let mut s = State::default();
        s.reconcile("m", &[incoming("10010")], now()).unwrap();
        let n = &s.pending[0];
        let sender = Sender::new().unwrap();
        let key = "test-secret-with-at-least-32-bytes";
        assert!(deliver(&sender, key, &url, n).await.is_err());
        assert!(deliver(&sender, key, &url, n).await.is_err());
        deliver(&sender, key, &url, n).await.unwrap();
        server.abort();
    }
}
