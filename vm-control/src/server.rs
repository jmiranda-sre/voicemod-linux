//! Async WebSocket server: bind-All + port scan + auth hook.
//!
//! Mirrors the reference `WebSocketServer::listen` behaviour: iterate a
//! candidate port list, bind the first free one, remember the chosen port.
//! Default bind is all interfaces (LAN pairing); loopback-only is one flag.

use super::actions::{Action, ControlMessage, ControlReply};
use futures_util::{SinkExt, StreamExt};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Bind address. `0.0.0.0` = LAN pairing (reference default).
    pub bind: IpAddr,
    /// Candidate ports, tried in order. The effective port is reported.
    pub ports: Vec<u16>,
    /// If true, every message must carry the expected token.
    pub require_auth: bool,
    /// Expected bearer token when `require_auth` (never logged).
    pub expected_token: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            // Runtime-scanned like the reference; exact vendor defaults
            // are confirmed against live traffic in Fase 4.
            ports: vec![8765, 8766, 8767, 8768, 8769],
            require_auth: false,
            expected_token: None,
        }
    }
}

impl ServerConfig {
    pub fn loopback(mut self) -> Self {
        self.bind = IpAddr::V4(Ipv4Addr::LOCALHOST);
        self
    }
}

/// Resolve the expected bearer token when auth is required.
/// Reads `VM_CONTROL_TOKEN`; fails fast when required-but-unset so a
/// "secured" server can never boot open by accident.
fn expected_token(config: &ServerConfig) -> Result<Option<String>, ServerError> {
    if !config.require_auth {
        return Ok(None);
    }
    match std::env::var("VM_CONTROL_TOKEN") {
        Ok(t) if !t.is_empty() => Ok(Some(t)),
        _ => Err(ServerError::Protocol(
            "auth required but VM_CONTROL_TOKEN is unset (refusing to serve open)".into(),
        )),
    }
}

#[derive(Debug)]
pub enum ServerError {
    Io(std::io::Error),
    NoPort,
    Protocol(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "ws i/o: {e}"),
            Self::NoPort => write!(f, "no port available for connection"),
            Self::Protocol(e) => write!(f, "protocol: {e}"),
        }
    }
}

impl std::error::Error for ServerError {}

/// Dispatch one action through the live handler. Pure function over
/// the transport: unit-testable without sockets.
pub fn dispatch(handler: &dyn super::actions::ControlHandler, action: &Action) -> ControlReply {
    handler.handle(action).0
}

pub struct ControlApiServer {
    config: ServerConfig,
    events: broadcast::Sender<String>,
    handler: Arc<dyn super::actions::ControlHandler>,
}

impl ControlApiServer {
    pub fn new(config: ServerConfig, handler: Arc<dyn super::actions::ControlHandler>) -> Self {
        let (events, _) = broadcast::channel(64);
        Self { config, events, handler }
    }

    /// Bind the first free candidate port and serve until cancelled.
    /// Returns the effective port (mirrors "using port [{}]").
    pub async fn serve(&self) -> Result<u16, ServerError> {
        let expected = expected_token(&self.config)?;
        let mut listener = None;
        let mut chosen = 0;
        for port in &self.config.ports {
            match TcpListener::bind((self.config.bind, *port)).await {
                Ok(l) => {
                    listener = Some(l);
                    chosen = *port;
                    break;
                }
                // Backlog item (v0.1.0): a second instance died silently
                // on bind. Say exactly which port was busy and that we
                // move on — the effective port is still reported below.
                Err(e) => {
                    eprintln!("control api: port {port} busy ({e}); trying next candidate");
                    continue;
                }
            }
        }
        let listener = listener.ok_or(ServerError::NoPort)?;
        let events = self.events.clone();
        let handler = self.handler.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let events = events.clone();
                let handler = handler.clone();
                let expected = expected.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(stream, events, handler, expected).await {
                        eprintln!("control connection error: {e}");
                    }
                });
            }
        });
        Ok(chosen)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.events.subscribe()
    }

    /// Clone of the event bus sender: lets the daemon's non-WS dispatch
    /// paths (hotkeys, MIDI, OBS, hotplug autoswitch) broadcast the same
    /// `ControlEvent` JSON string the server sends for API requests.
    pub fn publisher(&self) -> broadcast::Sender<String> {
        self.events.clone()
    }
}

async fn handle(
    stream: tokio::net::TcpStream,
    events: broadcast::Sender<String>,
    handler: Arc<dyn super::actions::ControlHandler>,
    expected_token: Option<String>,
) -> Result<(), ServerError> {
    let mut ws = tokio_tungstenite::accept_async(stream)
        .await
        .map_err(|e| ServerError::Protocol(e.to_string()))?;
    let mut sub = events.subscribe();
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(msg) = incoming else { break };
                let msg = msg.map_err(|e| ServerError::Protocol(e.to_string()))?;
                if !msg.is_text() {
                    continue;
                }
                let (reply_text, evts) =
                    match serde_json::from_str::<ControlMessage>(msg.to_text().unwrap_or("")) {
                        Ok(req) => {
                            let id = req.id.clone();
                            let (reply, evts) = if let Some(expected) = expected_token.as_deref() {
                                if req.token.as_deref() != Some(expected) {
                                    (ControlReply::err("unauthorized"), Vec::new())
                                } else {
                                    handler.handle(&req.action)
                                }
                            } else {
                                handler.handle(&req.action)
                            };
                            // Echo the request id for strict client correlation.
                            // The wire shape stays flat: {"ok":..,"data":..,"id":..}.
                            let mut value = serde_json::to_value(&reply).unwrap_or(
                                serde_json::json!({"ok": false, "error": "encode"}),
                            );
                            if let (Some(id), Some(obj)) = (id, value.as_object_mut()) {
                                obj.insert("id".to_string(), id);
                            }
                            (
                                serde_json::to_string(&value).unwrap_or_else(|_| {
                                    r#"{"ok":false,"error":"encode"}"#.to_string()
                                }),
                                evts,
                            )
                        }
                        Err(e) => (
                            serde_json::to_string(&ControlReply::err(format!(
                                "bad message: {e}"
                            )))
                            .unwrap_or_else(|_| r#"{"ok":false}"#.to_string()),
                            Vec::new(),
                        ),
                    };
                for evt in evts {
                    if let Ok(text) = serde_json::to_string(&evt) {
                        let _ = events.send(text);
                    }
                }
                ws.send(tokio_tungstenite::tungstenite::Message::Text(reply_text.into()))
                    .await
                    .map_err(|e| ServerError::Protocol(e.to_string()))?;
            }
            evt = sub.recv() => {
                // Broadcasts (including our own actions) fan out to every client.
                if let Ok(text) = evt {
                    if ws
                        .send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};

    struct Canned;
    impl super::super::actions::ControlHandler for Canned {
        fn handle(
            &self,
            action: &Action,
        ) -> (ControlReply, Vec<super::super::actions::ControlEvent>) {
            match action {
                Action::GetAudioStreamStatus => (
                    ControlReply::ok_data(serde_json::json!({"active": true})),
                    vec![super::super::actions::ControlEvent::StreamStatus { active: true }],
                ),
                _ => (ControlReply::err("canned: unknown"), Vec::new()),
            }
        }
    }

    #[tokio::test]
    async fn ws_roundtrip_with_handler_and_event() {
        let server = ControlApiServer::new(
            ServerConfig { bind: "127.0.0.1".parse().unwrap(), ports: vec![18765, 18775, 18785], require_auth: false, expected_token: None },
            Arc::new(Canned),
        );
        let port = server.serve().await.expect("serve");
        assert!([18765, 18775, 18785].contains(&port));
        let mut events = server.subscribe();

        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .expect("connect");
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"action":"getAudioStreamStatus"}"#.into(),
        ))
        .await
        .expect("send");
        let reply = ws.next().await.expect("reply").expect("msg");
        let v: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["data"]["active"], true);
        // The broadcast event follows on the subscription.
        let evt = tokio::time::timeout(std::time::Duration::from_secs(3), events.recv())
            .await
            .expect("event timeout")
            .expect("event");
        assert!(evt.contains("streamStatus") || evt.contains("StreamStatus"));
        ws.close(None).await.ok();
    }

    #[tokio::test]
    async fn busy_first_port_falls_through_to_next() {
        // Occupy the first candidate: serve must skip it loudly and bind
        // the second (backlog: second instance used to die silently).
        let _guard =
            std::net::TcpListener::bind("127.0.0.1:18768").expect("occupy first candidate");
        let server = ControlApiServer::new(
            ServerConfig { bind: "127.0.0.1".parse().unwrap(), ports: vec![18768, 18778], require_auth: false, expected_token: None },
            Arc::new(Canned),
        );
        let port = server.serve().await.expect("serve");
        assert_eq!(port, 18778);
    }

    #[tokio::test]
    async fn ws_bad_message_errors() {
        let server = ControlApiServer::new(
            ServerConfig { bind: "127.0.0.1".parse().unwrap(), ports: vec![18766, 18776, 18786], require_auth: false, expected_token: None },
            Arc::new(Canned),
        );
        let port = server.serve().await.expect("serve");
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .expect("connect");
        ws.send(tokio_tungstenite::tungstenite::Message::Text("not json".into()))
            .await
            .expect("send");
        let reply = ws.next().await.expect("reply").expect("msg");
        let v: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(v["ok"], false);
        ws.close(None).await.ok();
    }
}

#[cfg(test)]
mod auth_tests {
    use super::*;

    struct OkHandler;
    impl super::super::actions::ControlHandler for OkHandler {
        fn handle(
            &self,
            _action: &Action,
        ) -> (ControlReply, Vec<super::super::actions::ControlEvent>) {
            (ControlReply::ok(), Vec::new())
        }
    }

    fn cfg(port: u16, auth: bool) -> ServerConfig {
        ServerConfig { bind: "127.0.0.1".parse().unwrap(), ports: vec![port], require_auth: auth, expected_token: None }
    }

    async fn send_recv(port: u16, text: &str) -> serde_json::Value {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .expect("connect");
        ws.send(tokio_tungstenite::tungstenite::Message::Text(text.into()))
            .await
            .expect("send");
        let msg = ws.next().await.expect("reply").expect("msg");
        let v: serde_json::Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
        ws.close(None).await.ok();
        v
    }

    #[tokio::test]
    async fn auth_open_by_default() {
        let server = ControlApiServer::new(cfg(18770, false), Arc::new(OkHandler));
        let port = server.serve().await.expect("serve");
        let v = send_recv(port, r#"{"action":"getAudioStreamStatus"}"#).await;
        assert_eq!(v["ok"], true);
    }

    #[tokio::test]
    async fn auth_enforced_and_refuses_unset_token() {
        // Required but unset -> fail fast, never serve open.
        let server = ControlApiServer::new(cfg(18771, true), Arc::new(OkHandler));
        assert!(server.serve().await.is_err());

        // With token: wrong denied, right accepted.
        std::env::set_var("VM_CONTROL_TOKEN", "s3cret-test");
        let server = ControlApiServer::new(cfg(18772, true), Arc::new(OkHandler));
        let port = server.serve().await.expect("serve");
        let v = send_recv(port, r#"{"action":"getAudioStreamStatus"}"#).await;
        assert_eq!(v["ok"], false);
        assert_eq!(v["error"], "unauthorized");
        let v = send_recv(port, r#"{"action":"getAudioStreamStatus","token":"s3cret-test"}"#).await;
        assert_eq!(v["ok"], true);
        std::env::remove_var("VM_CONTROL_TOKEN");
    }
}

#[cfg(test)]
mod id_tests {
    use super::*;

    struct Echo;
    impl super::super::actions::ControlHandler for Echo {
        fn handle(
            &self,
            action: &Action,
        ) -> (ControlReply, Vec<super::super::actions::ControlEvent>) {
            let _ = action;
            (ControlReply::ok(), Vec::new())
        }
    }

    #[tokio::test]
    async fn reply_echoes_request_id() {
        use futures_util::{SinkExt, StreamExt};
        let server = ControlApiServer::new(
            ServerConfig {
                bind: "127.0.0.1".parse().unwrap(),
                ports: vec![18767, 18777, 18787],
                require_auth: false,
                expected_token: None,
            },
            Arc::new(Echo),
        );
        let port = server.serve().await.expect("serve");
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}"))
            .await
            .expect("connect");
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"action":"getAudioStreamStatus","id":42}"#.into(),
        ))
        .await
        .expect("send");
        let reply = ws.next().await.expect("reply").expect("msg");
        let v: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["id"], 42);
        // And without id: flat reply, no id key.
        ws.send(tokio_tungstenite::tungstenite::Message::Text(
            r#"{"action":"getAudioStreamStatus"}"#.into(),
        ))
        .await
        .expect("send");
        let reply = ws.next().await.expect("reply").expect("msg");
        let v: serde_json::Value = serde_json::from_str(reply.to_text().unwrap()).unwrap();
        assert_eq!(v["ok"], true);
        assert!(v.get("id").is_none());
        ws.close(None).await.ok();
    }
}
