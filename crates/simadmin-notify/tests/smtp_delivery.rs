//! SMTP contract tests use only an ephemeral loopback listener and .invalid mailboxes.
use serde_json::json;
use simadmin_notify::{ChannelType, NotificationMessage, Sender};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::time::{timeout, Duration};

async fn smtp_fixture(
    reject_recipient: bool,
) -> (u16, tokio::task::JoinHandle<(Vec<String>, String)>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(10), async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(stream);
            socket
                .get_mut()
                .write_all(b"220 localhost test SMTP\r\n")
                .await
                .unwrap();
            let mut recipients = Vec::new();
            let mut data = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(socket.read_line(&mut line).await.unwrap(), 0);
                let response: &[u8] = if line.starts_with("EHLO ") || line.starts_with("HELO ") {
                    b"250 localhost\r\n"
                } else if line.starts_with("MAIL FROM:") {
                    assert!(line.contains("sender@example.invalid"));
                    b"250 sender accepted\r\n"
                } else if line.starts_with("RCPT TO:") {
                    recipients.push(line.trim().to_string());
                    if reject_recipient {
                        socket
                            .get_mut()
                            .write_all(b"550 recipient rejected\r\n")
                            .await
                            .unwrap();
                        return (recipients, data);
                    }
                    b"250 recipient accepted\r\n"
                } else if line == "DATA\r\n" {
                    socket
                        .get_mut()
                        .write_all(b"354 end with dot\r\n")
                        .await
                        .unwrap();
                    loop {
                        let mut body_line = String::new();
                        assert_ne!(socket.read_line(&mut body_line).await.unwrap(), 0);
                        if body_line == ".\r\n" {
                            break;
                        }
                        data.push_str(body_line.strip_prefix('.').unwrap_or(&body_line));
                    }
                    socket
                        .get_mut()
                        .write_all(b"250 queued locally\r\n")
                        .await
                        .unwrap();
                    return (recipients, data);
                } else {
                    panic!("unexpected SMTP command: {line}");
                };
                socket.get_mut().write_all(response).await.unwrap();
            }
        })
        .await
        .expect("local SMTP fixture timed out")
    });
    (port, server)
}

fn decoded_body(mail: &str) -> String {
    use base64::Engine;
    let (headers, body) = mail.split_once("\r\n\r\n").unwrap();
    let lower = headers.to_ascii_lowercase();
    if lower.contains("content-transfer-encoding: base64") {
        return String::from_utf8(
            base64::engine::general_purpose::STANDARD
                .decode(body.split_whitespace().collect::<String>())
                .unwrap(),
        )
        .unwrap();
    }
    if lower.contains("content-transfer-encoding: quoted-printable") {
        let joined = body.replace("=\r\n", "");
        let mut bytes = joined.bytes();
        let mut decoded = Vec::new();
        while let Some(byte) = bytes.next() {
            if byte == b'=' {
                let pair = [bytes.next().unwrap(), bytes.next().unwrap()];
                decoded.push(u8::from_str_radix(std::str::from_utf8(&pair).unwrap(), 16).unwrap());
            } else {
                decoded.push(byte);
            }
        }
        return String::from_utf8(decoded).unwrap();
    }
    body.to_owned()
}

#[tokio::test]
async fn forwards_unicode_sms_body_to_all_configured_mailboxes() {
    let (port, server) = smtp_fixture(false).await;
    let body = "中国联通短信：验证码 123456。\n第二行内容。";
    let receipt = timeout(Duration::from_secs(10), Sender::new().unwrap().send(
        ChannelType::Email,
        &json!({"smtp_host":"127.0.0.1", "smtp_port":port, "smtp_security":"none", "sender_address":"sender@example.invalid", "receiver_addresses":"first@example.invalid; second@example.invalid", "message_format":"plain"}),
        &NotificationMessage { title:"收到短信".into(), body:body.into(), custom_body:None }
    )).await.unwrap().unwrap();
    assert_eq!(receipt.provider, "email");
    assert_eq!(receipt.status_code, 250);
    let (recipients, mail) = server.await.unwrap();
    assert_eq!(
        recipients,
        [
            "RCPT TO:<first@example.invalid>",
            "RCPT TO:<second@example.invalid>"
        ]
    );
    assert!(mail
        .to_ascii_lowercase()
        .contains("content-type: text/plain; charset=utf-8"));
    assert_eq!(decoded_body(&mail).replace("\r\n", "\n").trim_end(), body);
}

#[tokio::test]
async fn rejected_smtp_recipient_is_reported_as_delivery_failure() {
    let (port, server) = smtp_fixture(true).await;
    let result = timeout(Duration::from_secs(10), Sender::new().unwrap().send(
        ChannelType::Email,
        &json!({"smtp_host":"127.0.0.1", "smtp_port":port, "smtp_security":"none", "sender_address":"sender@example.invalid", "receiver_addresses":"rejected@example.invalid"}),
        &NotificationMessage { title:"SMS".into(), body:"test".into(), custom_body:None }
    )).await.unwrap();
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("550"));
    let (_, data) = server.await.unwrap();
    assert!(data.is_empty());
}
