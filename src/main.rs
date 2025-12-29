// Sendspin Rust CLI Player - Simplified Architecture
//
// Design principles:
// 1. Audio IN → Decode → Simple Queue (VecDeque)
// 2. Audio OUT → Time-synced playback from queue
// 3. Stop → Clear queue + drop output (instant)
// 4. Skip → Stop old + Start new (clean transition)
// 5. All output is time-synced to play_at timestamps

mod legacy_protocol;
mod player;

use clap::Parser;
use log::{debug, error, info};
use player::Player;
use sendspin::audio::decode::{Decoder, PcmDecoder, PcmEndian};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec};
use sendspin::protocol::messages::{AudioFormatSpec, ClientHello, ClientTime, DeviceInfo, PlayerV1Support};
use sendspin::sync::ClockSync;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::interval;

use legacy_protocol::{LegacyClient, LegacyMessage, LegacyPlayerState};

#[derive(Parser, Debug)]
#[command(name = "sendspin-rs-cli")]
#[command(about = "Connect to Music Assistant and play audio", long_about = None)]
struct Args {
    #[arg(short, long, default_value = "192.168.70.245:8927")]
    server: String,
    #[arg(short, long, default_value = "Sendspin-RS Player")]
    name: String,
    #[arg(long)]
    client_id: Option<String>,
    #[arg(short, long, default_value = "30")]
    volume: u8,
    #[arg(short, long, default_value = "20")]
    buffer: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let args = Args::parse();

    let client_id = args
        .client_id
        .clone()
        .unwrap_or_else(|| format!("sendspin-rs-{}", uuid::Uuid::new_v4()));

    info!("Client ID: {}", client_id);

    // Connect
    let ws_url = format!("ws://{}/sendspin", args.server);
    info!("Connecting to {}...", ws_url);

    let hello = ClientHello {
        client_id: client_id.clone(),
        name: args.name.clone(),
        version: 1,
        supported_roles: vec!["player@v1".to_string()],
        device_info: Some(DeviceInfo {
            product_name: Some(args.name.clone()),
            manufacturer: Some("Sendspin-RS".to_string()),
            software_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        }),
        player_v1_support: Some(PlayerV1Support {
            supported_formats: vec![
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 48000,
                    bit_depth: 24,
                },
                AudioFormatSpec {
                    codec: "pcm".to_string(),
                    channels: 2,
                    sample_rate: 48000,
                    bit_depth: 16,
                },
            ],
            buffer_capacity: 1048576,
            supported_commands: vec!["volume".to_string(), "mute".to_string()],
        }),
        artwork_v1_support: None,
        visualizer_v1_support: None,
    };

    let client = LegacyClient::connect(&ws_url, hello).await?;
    info!("Connected!");

    let (mut message_rx, mut audio_rx, ws_tx) = client.split();

    // Send initial state
    let initial_state = ClientState {
        player: LegacyPlayerState {
            state: "synchronized".to_string(),
            volume: args.volume,
            muted: false,
        },
    };
    ws_tx.send_message(initial_state).await?;
    info!("Sent initial client/state");

    // Clock sync
    let clock_sync = Arc::new(tokio::sync::Mutex::new(ClockSync::new()));

    // Send initial time sync
    let client_transmitted = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64;
    ws_tx
        .send_message(ClientTime { client_transmitted })
        .await?;

    // Periodic time sync
    let ws_tx_clone: legacy_protocol::LegacyWsSender = ws_tx.clone();
    tokio::spawn(async move {
        let mut interval = interval(Duration::from_secs(5));
        loop {
            interval.tick().await;
            let client_transmitted = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_micros() as i64;
            let _ = ws_tx_clone
                .send_message(Message::ClientTime { client_transmitted })
                .await;
        }
    });

    info!("Waiting for stream to start...");

    // Create player with initial volume
    let player = Player::new(args.volume);

    // Message handling
    let mut decoder: Option<PcmDecoder> = None;
    let mut audio_format: Option<AudioFormat> = None;
    let mut endian_locked: Option<PcmEndian> = None;
    let mut next_play_time: Option<Instant> = None;
    let buffer_ms = args.buffer;

    loop {
        tokio::select! {
            Some(msg) = message_rx.recv() => {
                match &msg {
                    Message::StreamStart(_) => info!("← SERVER: stream/start"),
                    Message::StreamEnd(_) => info!("← SERVER: stream/end"),
                    Message::StreamClear(_) => info!("← SERVER: stream/clear"),
                    Message::ServerCommand(cmd) => info!("← SERVER: command {:?}", cmd),
                    _ => {}
                }

                match msg {
                    Message::StreamStart(stream_start) => {
                        if let Some(player_info) = stream_start.get("player") {
                            let codec = player_info.get("codec").and_then(|v| v.as_str()).unwrap_or("unknown");
                            let sample_rate = player_info.get("sample_rate").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let channels = player_info.get("channels").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                            let bit_depth = player_info.get("bit_depth").and_then(|v| v.as_u64()).unwrap_or(0) as u32;

                            if codec != "pcm" || (bit_depth != 16 && bit_depth != 24) {
                                error!("Unsupported format");
                                continue;
                            }

                            // New stream: Stop old, setup new, Resume
                            player.stop();
                            std::thread::sleep(Duration::from_millis(5)); // Give time to clear
                            player.resume();

                            audio_format = Some(AudioFormat {
                                codec: Codec::Pcm,
                                sample_rate,
                                channels: channels as u8,
                                bit_depth: bit_depth as u8,
                                codec_header: None,
                            });

                            decoder = None;
                            endian_locked = None;
                            next_play_time = None;

                            debug!("Stream: {}Hz {}ch {}bit", sample_rate, channels, bit_depth);

                            // Send playing state to server
                            let state = Message::ClientState {
                                player: LegacyPlayerState {
                                    state: "playing".to_string(),
                                    volume: args.volume,
                                    muted: false,
                                },
                            };
                            let _ = ws_tx.send_message(state).await;
                        }
                    }
                    Message::StreamEnd(end_data) => {
                        // Check if there's more context in the stream/end message
                        info!("← stream/end data: {:?}", end_data);

                        // For now, stop playback on stream/end
                        // This handles both pause and end-of-track cases
                        player.stop();
                        next_play_time = None;

                        // Send paused state to server
                        let state = LegacyMessage::ClientState {
                            player: LegacyPlayerState {
                                state: "paused".to_string(),
                                volume: args.volume,
                                muted: false,
                            },
                        };
                        let _ = ws_tx.send_message(state).await;
                    }
                    Message::StreamClear(_) => {
                        player.stop();
                        decoder = None;
                        audio_format = None;
                        endian_locked = None;
                        next_play_time = None;

                        // Send paused state to server
                        let state = Message::ClientState {
                            player: LegacyPlayerState {
                                state: "paused".to_string(),
                                volume: args.volume,
                                muted: false,
                            },
                        };
                        let _ = ws_tx.send_message(state).await;
                    }
                    Message::ServerCommand(command) => {
                        // Check if this is a player command
                        if let Some(player_cmd) = command.get("player").and_then(|v| v.as_object()) {
                            if let Some(cmd) = player_cmd.get("command").and_then(|v| v.as_str()) {
                                match cmd {
                                    "pause" | "stop" => {
                                        info!("→ Handling pause/stop command");
                                        player.stop();
                                        // Send paused state to server
                                        let state = Message::ClientState {
                                            player: LegacyPlayerState {
                                                state: "paused".to_string(),
                                                volume: args.volume,
                                                muted: false,
                                            },
                                        };
                                        let _ = ws_tx.send_message(state).await;
                                    }
                                    "play" => {
                                        player.resume();
                                        // Send playing state to server
                                        let state = Message::ClientState {
                                            player: LegacyPlayerState {
                                                state: "playing".to_string(),
                                                volume: args.volume,
                                                muted: false,
                                            },
                                        };
                                        let _ = ws_tx.send_message(state).await;
                                    }
                                    "volume" => {
                                        if let Some(vol) = player_cmd.get("volume").and_then(|v| v.as_u64()) {
                                            info!("← Setting volume to {}", vol);
                                            player.set_volume(vol as u8);
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    Message::ServerTime { client_transmitted, server_received, server_transmitted } => {
                        let t4 = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as i64;
                        clock_sync.lock().await.update(client_transmitted, server_received, server_transmitted, t4);
                    }
                    _ => {}
                }
            }

            Some(chunk) = audio_rx.recv() => {
                if let Some(ref fmt) = audio_format {
                    if endian_locked.is_none() {
                        endian_locked = Some(PcmEndian::Little);
                        decoder = Some(PcmDecoder::with_endian(fmt.bit_depth, PcmEndian::Little));
                    }
                }

                if let (Some(ref dec), Some(ref fmt)) = (&decoder, &audio_format) {
                    if let Ok(samples) = dec.decode(&chunk.data) {
                        let frames = samples.len() / fmt.channels as usize;
                        let duration = Duration::from_micros(
                            (frames as u64 * 1_000_000) / fmt.sample_rate as u64
                        );

                        // Determine play time
                        let sync = clock_sync.lock().await;
                        let play_at = if let Some(instant) = sync.server_to_local_instant(chunk.timestamp) {
                            instant
                        } else {
                            // Fallback timing
                            if next_play_time.is_none() {
                                next_play_time = Some(Instant::now() + Duration::from_millis(buffer_ms));
                            }
                            let pt = next_play_time.unwrap();
                            next_play_time = Some(pt + duration);
                            pt
                        };
                        drop(sync);

                        let buffer = AudioBuffer {
                            timestamp: chunk.timestamp,
                            play_at,
                            samples,
                            format: fmt.clone(),
                        };

                        // Add to player queue
                        player.enqueue(buffer);
                    }
                }
            }

            else => break,
        }
    }

    Ok(())
}
