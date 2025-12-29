// Compatibility shim for Music Assistant server
// Handles field name differences between sendspin-rs library and MA server

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use log::{debug, error, info};
use sendspin::protocol::messages::{ClientHello, Message};
use sendspin::sync::ClockSync;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio_tungstenite::{
    connect_async, tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream,
};

/// WebSocket sender wrapper (local version for compatibility)
pub struct CompatWsSender {
    tx: Arc<tokio::sync::Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMessage>>>,
}

impl CompatWsSender {
    /// Send a message to the server
    pub async fn send_message(&self, msg: Message) -> Result<(), Box<dyn std::error::Error>> {
        let json = serde_json::to_string(&msg)?;
        debug!("Sending message: {}", json);

        let mut tx = self.tx.lock().await;
        tx.send(WsMessage::Text(json)).await?;
        Ok(())
    }
}

/// Connect to Music Assistant server with field name compatibility fixes
pub async fn connect_with_compat(
    url: &str,
    hello: ClientHello,
) -> Result<
    (
        UnboundedReceiver<Message>,
        UnboundedReceiver<sendspin::protocol::client::AudioChunk>,
        Arc<tokio::sync::Mutex<ClockSync>>,
        CompatWsSender,
    ),
    Box<dyn std::error::Error>,
> {
    // Connect WebSocket manually
    let (ws_stream, _) = connect_async(url).await?;
    let (mut write, read) = ws_stream.split();

    // Serialize the ClientHello normally
    let hello_msg = Message::ClientHello(hello);
    let mut hello_json = serde_json::to_value(&hello_msg)?;

    // Fix field names for Music Assistant compatibility
    if let Some(payload) = hello_json.get_mut("payload") {
        let payload_obj = payload.as_object_mut().unwrap();

        // Rename player@v1_support to player_support
        if let Some(player_v1_support) = payload_obj.get("player@v1_support").cloned() {
            payload_obj.insert("player_support".to_string(), player_v1_support);
            payload_obj.remove("player@v1_support");
        }

        // Rename artwork@v1_support to artwork_support if present
        if let Some(artwork_v1_support) = payload_obj.get("artwork@v1_support").cloned() {
            payload_obj.insert("artwork_support".to_string(), artwork_v1_support);
            payload_obj.remove("artwork@v1_support");
        }

        // Rename visualizer@v1_support to visualizer_support if present
        if let Some(visualizer_v1_support) = payload_obj.get("visualizer@v1_support").cloned() {
            payload_obj.insert("visualizer_support".to_string(), visualizer_v1_support);
            payload_obj.remove("visualizer@v1_support");
        }
    }

    let hello_string = serde_json::to_string(&hello_json)?;
    debug!("Sending compatibility hello: {}", hello_string);

    // Send modified hello
    write.send(WsMessage::Text(hello_string)).await?;

    // Wait for server hello
    let mut read_temp = read;
    debug!("Waiting for server/hello...");

    loop {
        if let Some(result) = read_temp.next().await {
            match result {
                Ok(WsMessage::Text(text)) => {
                    debug!("Received text message: {}", text);
                    let msg: Message = serde_json::from_str(&text)?;

                    match msg {
                        Message::ServerHello(server_hello) => {
                            info!(
                                "Connected to server: {} ({})",
                                server_hello.name, server_hello.server_id
                            );
                            break;
                        }
                        _ => {
                            error!("Expected server/hello, got: {:?}", msg);
                            return Err("Expected server/hello".into());
                        }
                    }
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                    debug!("Received Ping/Pong, continuing to wait for server/hello");
                    continue;
                }
                Ok(WsMessage::Close(_)) => {
                    error!("Server closed connection");
                    return Err("Server closed connection".into());
                }
                Ok(other) => {
                    debug!("Unexpected message type: {:?}", other);
                    continue;
                }
                Err(e) => {
                    error!("WebSocket error: {}", e);
                    return Err(e.into());
                }
            }
        } else {
            error!("Connection closed before receiving server/hello");
            return Err("No server hello received".into());
        }
    }

    // Now create the normal ProtocolClient infrastructure
    // We need to reconstruct the client state with the existing connection
    use tokio::sync::mpsc::unbounded_channel;

    let (audio_tx, audio_rx) = unbounded_channel();
    let (artwork_tx, _artwork_rx) = unbounded_channel();
    let (visualizer_tx, _visualizer_rx) = unbounded_channel();
    let (message_tx, message_rx) = unbounded_channel();

    let clock_sync = Arc::new(tokio::sync::Mutex::new(ClockSync::new()));
    let clock_sync_clone = Arc::clone(&clock_sync);

    // Spawn message router
    tokio::spawn(async move {
        message_router(
            read_temp,
            audio_tx,
            artwork_tx,
            visualizer_tx,
            message_tx,
            clock_sync_clone,
        )
        .await;
    });

    let ws_sender = CompatWsSender {
        tx: Arc::new(tokio::sync::Mutex::new(write)),
    };

    Ok((message_rx, audio_rx, clock_sync, ws_sender))
}

// Copy of message_router from ProtocolClient
async fn message_router(
    mut read: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    audio_tx: tokio::sync::mpsc::UnboundedSender<sendspin::protocol::client::AudioChunk>,
    artwork_tx: tokio::sync::mpsc::UnboundedSender<sendspin::protocol::client::ArtworkChunk>,
    visualizer_tx: tokio::sync::mpsc::UnboundedSender<sendspin::protocol::client::VisualizerChunk>,
    message_tx: tokio::sync::mpsc::UnboundedSender<Message>,
    _clock_sync: Arc<tokio::sync::Mutex<ClockSync>>,
) {
    use sendspin::protocol::client::BinaryFrame;

    while let Some(msg) = read.next().await {
        match msg {
            Ok(WsMessage::Binary(data)) => {
                debug!("Received binary frame ({} bytes)", data.len());
                match BinaryFrame::from_bytes(&data) {
                    Ok(BinaryFrame::Audio(chunk)) => {
                        debug!(
                            "Parsed audio chunk: timestamp={}, data_len={}",
                            chunk.timestamp,
                            chunk.data.len()
                        );
                        let _ = audio_tx.send(chunk);
                    }
                    Ok(BinaryFrame::Artwork(chunk)) => {
                        debug!(
                            "Parsed artwork chunk: channel={}, timestamp={}, data_len={}",
                            chunk.channel,
                            chunk.timestamp,
                            chunk.data.len()
                        );
                        let _ = artwork_tx.send(chunk);
                    }
                    Ok(BinaryFrame::Visualizer(chunk)) => {
                        debug!(
                            "Parsed visualizer chunk: timestamp={}, data_len={}",
                            chunk.timestamp,
                            chunk.data.len()
                        );
                        let _ = visualizer_tx.send(chunk);
                    }
                    Ok(BinaryFrame::Unknown { type_id, .. }) => {
                        debug!("Received unknown binary type: {}", type_id);
                    }
                    Err(e) => {
                        debug!("Failed to parse binary frame: {}", e);
                    }
                }
            }
            Ok(WsMessage::Text(text)) => {
                debug!("Received text message: {}", text);
                match serde_json::from_str::<Message>(&text) {
                    Ok(msg) => {
                        debug!("Parsed message: {:?}", msg);
                        let _ = message_tx.send(msg);
                    }
                    Err(e) => {
                        debug!("Failed to parse message: {}", e);
                    }
                }
            }
            Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                // Handled automatically
            }
            Ok(WsMessage::Close(_)) => {
                info!("Server closed connection");
                break;
            }
            Err(e) => {
                error!("WebSocket error: {}", e);
                break;
            }
            _ => {}
        }
    }
}
