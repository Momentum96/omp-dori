//! The WebSocket transport: `wss://<api>/v1/live/sessions`, `session.start` first, audio as base64
//! PCM in `session.input_audio.append` and `session.output_audio.delta`.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use super::{Endpoint, Link, LinkEvent, Out, RealtimeLink, SESSIONS_PATH, classify};
use crate::audio::samples;
use crate::host::VoiceHost;

/// Handshake deadline after the connection is open.
const HANDSHAKE: Duration = Duration::from_secs(10);
/// One write may take this long.
const WRITE: Duration = Duration::from_secs(5);
/// Largest provider message.
const FRAME_MAX: usize = 16 * 1024 * 1024;

/// The WebSocket transport.
pub struct WebSocket;

impl RealtimeLink for WebSocket {
    async fn open<H: VoiceHost>(
        host: &Arc<H>,
        endpoint: &Endpoint,
        key: &str,
        session: Value,
    ) -> Result<Link, String> {
        let io = host
            .connect(endpoint.tls, &endpoint.host, endpoint.port)
            .await?;
        let scheme = if endpoint.tls { "wss" } else { "ws" };
        let url = format!(
            "{scheme}://{}:{}{SESSIONS_PATH}",
            endpoint.host, endpoint.port
        );
        let mut request = url
            .into_client_request()
            .map_err(|e| format!("bad realtime URL: {e}"))?;
        let auth = HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| "the key holds characters a header cannot carry".to_string())?;
        request.headers_mut().insert("authorization", auth);
        let config = WebSocketConfig::default()
            .max_message_size(Some(FRAME_MAX))
            .max_frame_size(Some(FRAME_MAX));
        let (mut ws, _) = timeout(
            HANDSHAKE,
            tokio_tungstenite::client_async_with_config(request, io, Some(config)),
        )
        .await
        .map_err(|_| "realtime handshake timed out".to_string())?
        .map_err(|e| format!("realtime handshake failed: {}", short(&e.to_string())))?;
        let start =
            json!({"type": "session.start", "event_id": "cataly_start", "session": session});
        timeout(WRITE, ws.send(Message::text(start.to_string())))
            .await
            .map_err(|_| "session.start write timed out".to_string())?
            .map_err(|e| format!("session.start write failed: {e}"))?;
        let (mut link, mut out_rx, in_tx) = Link::new();
        link.task = tokio::spawn(async move {
            let reason = loop {
                tokio::select! {
                    out = out_rx.recv() => {
                        let msg = match out {
                            None | Some(Out::Close) => break "closed by the session".to_string(),
                            Some(Out::Event(v)) => v.to_string(),
                            Some(Out::Audio(pcm)) => json!({
                                "type": "session.input_audio.append",
                                "audio": crate::b64::encode(&pcm),
                            })
                            .to_string(),
                        };
                        match timeout(WRITE, ws.send(Message::text(msg))).await {
                            Ok(Ok(())) => {}
                            Ok(Err(e)) => break format!("write failed: {e}"),
                            Err(_) => break "write took over 5 s".into(),
                        }
                    }
                    incoming = ws.next() => {
                        let event = match incoming {
                            None => break "the provider closed the socket".into(),
                            Some(Err(e)) => break format!("socket error: {}", short(&e.to_string())),
                            Some(Ok(Message::Close(c))) => {
                                break format!("closed by the provider ({})",
                                    c.map_or(0, |c| u16::from(c.code)));
                            }
                            Some(Ok(Message::Text(text))) => decode(text.as_str()),
                            Some(Ok(Message::Binary(_))) => LinkEvent::Malformed,
                            Some(Ok(_)) => continue,
                        };
                        if in_tx.send(event).await.is_err() {
                            break "session gone".into();
                        }
                    }
                }
            };
            let _ = in_tx.send(LinkEvent::Closed(reason)).await;
            let _ = timeout(WRITE, ws.close(None)).await;
        });
        Ok(link)
    }
}

/// Output audio becomes samples; everything else stays an event.
fn decode(text: &str) -> LinkEvent {
    match classify(text) {
        LinkEvent::Event(v)
            if v.get("type").and_then(Value::as_str) == Some("session.output_audio.delta") =>
        {
            match v
                .get("delta")
                .and_then(Value::as_str)
                .and_then(crate::b64::decode)
            {
                Some(bytes) => LinkEvent::Audio(samples(&bytes)),
                None => LinkEvent::Malformed,
            }
        }
        other => other,
    }
}

fn short(s: &str) -> String {
    s.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_audio_is_decoded() {
        let t = json!({"type": "session.output_audio.delta",
            "delta": crate::b64::encode(&[1, 0, 2, 0])})
        .to_string();
        assert!(matches!(decode(&t), LinkEvent::Audio(s) if s == vec![1, 2]));
        let bad = json!({"type": "session.output_audio.delta", "delta": "!"}).to_string();
        assert!(matches!(decode(&bad), LinkEvent::Malformed));
    }
}
