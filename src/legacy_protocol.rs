// Legacy protocol shim for Music Assistant compatibility
// Music Assistant expects a simpler protocol format than the current sendspin-rs library

use serde::{Deserialize, Serialize};
use tokio_tungstenite::{connect_async, tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream};
use tokio::net::TcpStream;
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;

// Import the sendspin protocol types
use sendspin::protocol::{Message, messages::ClientHello};

// =============================================================================
// Additional Protocol Types for ServerState and GroupUpdate
// =============================================================================

/// Playback state for group updates
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PlaybackState {
    Playing,
    Paused,
    Stopped,
}

/// Repeat mode
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RepeatMode {
    Off,
    One,
    All,
}

/// Track progress information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrackProgress {
    pub position: i64,
    pub duration: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_speed: Option<f64>,
}

/// Metadata state from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetadataState {
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artwork_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub year: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<TrackProgress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repeat: Option<RepeatMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shuffle: Option<bool>,
}

/// Controller state from server
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerState {
    pub supported_commands: Vec<String>,
    pub volume: u8,
    pub muted: bool,
}

/// Server state update (metadata and controller info)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyServerState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MetadataState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controller: Option<ControllerState>,
}

/// Group update notification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyGroupUpdate {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_state: Option<PlaybackState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_name: Option<String>,
}

// =============================================================================
// Legacy Client State
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegacyPlayerState {
    pub state: String,
    pub volume: u8,
    pub muted: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
pub enum LegacyMessage {
    #[serde(rename = "client/hello")]
    ClientHello(ClientHello),
    #[serde(rename = "server/hello")]
    ServerHello(serde_json::Value),
    #[serde(rename = "client/time")]
    ClientTime { client_transmitted: i64 },
    #[serde(rename = "server/time")]
    ServerTime {
        client_transmitted: i64,
        server_received: i64,
        server_transmitted: i64,
    },
    #[serde(rename = "player/update")]
    PlayerUpdate {
        state: String,
        volume: u8,
        muted: bool,
    },
    #[serde(rename = "client/state")]
    ClientState {
        player: LegacyPlayerState,
    },
    #[serde(rename = "stream/start")]
    StreamStart(serde_json::Value),
    #[serde(rename = "stream/clear")]
    StreamClear(serde_json::Value),
    #[serde(rename = "stream/end")]
    StreamEnd(serde_json::Value),
    #[serde(rename = "server/command")]
    ServerCommand(serde_json::Value),
    #[serde(rename = "server/state")]
    ServerState(LegacyServerState),
    #[serde(rename = "group/update")]
    GroupUpdate(LegacyGroupUpdate),
    #[serde(other)]
    Unknown,
}

pub struct LegacyAudioChunk {
    pub timestamp: i64,
    pub data: Arc<[u8]>,
}

impl LegacyAudioChunk {
    pub fn from_bytes(frame: &[u8]) -> Result<Self, String> {
        if frame.len() < 9 {
            return Err("Audio chunk too short".to_string());
        }

        // Music Assistant legacy format uses message type 4 for audio chunks
        // (not 0x01 like the newer sendspin protocol)
        if frame[0] != 4 {
            return Err(format!("Invalid audio chunk type: expected 4, got {}", frame[0]));
        }

        let timestamp = i64::from_be_bytes([
            frame[1], frame[2], frame[3], frame[4], frame[5], frame[6], frame[7], frame[8],
        ]);

        let data = Arc::from(&frame[9..]);

        Ok(Self { timestamp, data })
    }
}

pub struct LegacyClient {
    ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

impl LegacyClient {
    pub async fn connect(url: &str, hello: ClientHello) -> Result<Self, Box<dyn std::error::Error>> {
        // Connect WebSocket
        let (ws_stream, _) = connect_async(url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Send client hello
        let hello_msg = LegacyMessage::ClientHello(hello);
        let hello_json = serde_json::to_string(&hello_msg)?;

        log::debug!("Sending legacy client/hello: {}", hello_json);

        write.send(WsMessage::Text(hello_json)).await?;

        // Wait for server hello
        log::debug!("Waiting for server/hello...");

        loop {
            if let Some(result) = read.next().await {
                match result {
                    Ok(WsMessage::Text(text)) => {
                        log::debug!("Received text message: {}", text);
                        let msg: LegacyMessage = serde_json::from_str(&text)?;

                        match msg {
                            LegacyMessage::ServerHello(_) => {
                                log::info!("Received server/hello (legacy format)");
                                break;
                            }
                            _ => {
                                log::error!("Expected server/hello, got different message");
                                return Err("Expected server/hello".into());
                            }
                        }
                    }
                    Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                        log::debug!("Received Ping/Pong, continuing to wait for server/hello");
                        continue;
                    }
                    Ok(WsMessage::Close(_)) => {
                        return Err("Server closed connection".into());
                    }
                    Ok(other) => {
                        log::warn!("Unexpected message type while waiting for hello: {:?}", other);
                        continue;
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
            } else {
                return Err("Connection closed before receiving server/hello".into());
            }
        }

        // Reunite the stream
        let ws_stream = read.reunite(write).map_err(|e| format!("Failed to reunite stream: {:?}", e))?;

        Ok(Self { ws_stream })
    }

    pub fn split(self) -> (
        tokio::sync::mpsc::UnboundedReceiver<LegacyMessage>,
        tokio::sync::mpsc::UnboundedReceiver<LegacyAudioChunk>,
        LegacyWsSender,
    ) {
        let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel();
        let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel();

        let (write, read) = self.ws_stream.split();
        let sender = LegacyWsSender {
            tx: Arc::new(tokio::sync::Mutex::new(write)),
        };

        // Spawn message router
        tokio::spawn(async move {
            let mut read = read;
            while let Some(msg) = read.next().await {
                match msg {
                    Ok(WsMessage::Binary(data)) => {
                        log::debug!("Received binary frame ({} bytes)", data.len());
                        match LegacyAudioChunk::from_bytes(&data) {
                            Ok(chunk) => {
                                let _ = audio_tx.send(chunk);
                            }
                            Err(e) => {
                                log::warn!("Failed to parse audio chunk: {}", e);
                            }
                        }
                    }
                    Ok(WsMessage::Text(text)) => {
                        log::debug!("Received text message: {}", text);
                        match serde_json::from_str::<LegacyMessage>(&text) {
                            Ok(msg) => {
                                let _ = msg_tx.send(msg);
                            }
                            Err(e) => {
                                log::warn!("Failed to parse message: {} - Raw: {}", e, text);
                            }
                        }
                    }
                    Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                        // Handled automatically
                    }
                    Ok(WsMessage::Close(_)) => {
                        log::info!("Server closed connection");
                        break;
                    }
                    Err(e) => {
                        log::error!("WebSocket error: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        (msg_rx, audio_rx, sender)
    }
}

#[derive(Clone)]
pub struct LegacyWsSender {
    tx: Arc<tokio::sync::Mutex<futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>>>,
}

impl LegacyWsSender {
    pub async fn send_message(&self, msg: Message) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&msg)?;
        log::debug!("Sending message: {}", json);

        let mut tx = self.tx.lock().await;
        tx.send(WsMessage::Text(json)).await?;
        Ok(())
    }
}
