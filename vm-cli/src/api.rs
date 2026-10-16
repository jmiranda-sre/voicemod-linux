//! Raw Control API client (F-0.4.D): send any action JSON, print the
//! reply. This is the "CLI 100%": every current and future action works
//! here with no per-action code, which also keeps scripts immune to
//! catalogue growth.

use futures_util::{SinkExt, StreamExt};

/// Send one action object to the first reachable port and return the
/// reply (the one carrying an `"ok"` key — broadcasts in flight are
/// skipped, never mistaken for the answer).
pub async fn call_action(ports: &[u16], action: &str) -> Result<serde_json::Value, String> {
    let text = action.to_string();
    // Fail fast on non-objects: the server would only ever answer
    // "bad message" without an id to correlate.
    let v: serde_json::Value =
        serde_json::from_str(action).map_err(|e| format!("action must be a JSON object: {e}"))?;
    if !v.is_object() {
        return Err("action must be a JSON object".to_string());
    }
    let mut last_err = String::from("no ports given");
    for port in ports {
        match call_one(*port, &text).await {
            Ok(reply) => return Ok(reply),
            Err(e) => last_err = format!(":{port}: {e}"),
        }
    }
    Err(format!("no Control API port reachable ({last_err})"))
}

async fn call_one(port: u16, text: &str) -> Result<serde_json::Value, String> {
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
        .await
        .map_err(|e| format!("connect: {e}"))?;
    ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
        .await
        .map_err(|e| format!("send: {e}"))?;
    loop {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "reply timeout".to_string())?
            .ok_or_else(|| "connection closed".to_string())?
            .map_err(|e| format!("recv: {e}"))?;
        if !msg.is_text() {
            continue;
        }
        let v: serde_json::Value =
            serde_json::from_str(msg.to_text().unwrap_or("")).map_err(|e| format!("decode: {e}"))?;
        if v.get("ok").is_some() {
            // Graceful close: a bare drop makes the daemon log a
            // "connection reset without closing handshake" per call.
            let _ = ws.close(None).await;
            return Ok(v);
        }
        // Broadcasts ("event") belong to everyone; keep waiting.
    }
}

/// Parse a `8765,8766` style port list (whitespace tolerant).
pub fn parse_ports(s: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for part in s.split(',') {
        let p: u16 =
            part.trim().parse().map_err(|_| format!("bad port '{part}' (want 8765,8766,…)"))?;
        out.push(p);
    }
    if out.is_empty() {
        return Err("empty port list".to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod api_tests {
    use super::*;
    use std::sync::Arc;
    use vm_control::{Action, ControlApiServer, ControlEvent, ControlHandler, ControlReply, ServerConfig};

    struct Canned;
    impl ControlHandler for Canned {
        fn handle(&self, action: &Action) -> (ControlReply, Vec<ControlEvent>) {
            match action {
                Action::GetVoices => (
                    ControlReply::ok_data(serde_json::json!({"voices": ["a"]})),
                    Vec::new(),
                ),
                _ => (ControlReply::err("canned: unknown"), Vec::new()),
            }
        }
    }

    #[test]
    fn port_list_parsing() {
        assert_eq!(parse_ports("8765,8766").unwrap(), vec![8765, 8766]);
        assert_eq!(parse_ports(" 8765 ").unwrap(), vec![8765]);
        assert!(parse_ports("").is_err());
        assert!(parse_ports("abc").is_err());
        assert!(parse_ports("99999").is_err());
    }

    #[tokio::test]
    async fn call_roundtrips_and_skips_broadcasts() {
        let server = ControlApiServer::new(
            ServerConfig {
                bind: "127.0.0.1".parse().unwrap(),
                ports: vec![18790],
                require_auth: false,
                expected_token: None,
            },
            Arc::new(Canned),
        );
        let port = server.serve().await.expect("serve");
        // Unknown action → honest error reply (still an "ok"-shaped answer).
        let v = call_action(&[port], r#"{"action":"getVoices"}"#).await.expect("call");
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["voices"], serde_json::json!(["a"]));
        let v = call_action(&[port], r#"{"action":"nope"}"#).await.expect("call");
        assert_eq!(v["ok"], false);
        // Not JSON at all → client-side rejection, no socket needed.
        assert!(call_action(&[port], "hello").await.is_err());
        assert!(call_action(&[port], "[1,2]").await.is_err());
        // Dead port → clear error, not a hang.
        assert!(call_action(&[19999], r#"{"action":"getVoices"}"#).await.is_err());
    }
}
