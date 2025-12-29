// Sendspin Rust CLI Player - Full Protocol Implementation
//
// Design principles:
// 1. Audio IN → Decode → Simple Queue (VecDeque)
// 2. Audio OUT → Time-synced playback from queue
// 3. Stop → Clear queue + drop output (instant)
// 4. Skip → Stop old + Start new (clean transition)
// 5. All output is time-synced to play_at timestamps

mod compat;
mod mdns;
mod player;

use clap::Parser;
use log::{debug, error, info};
use player::Player;
use sendspin::audio::decode::{Decoder, PcmDecoder, PcmEndian};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientHello, ClientState, ClientTime, DeviceInfo, Message, PlayerState,
    PlayerSyncState, PlayerV1Support,
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Parser, Debug)]
#[command(name = "sendspin-rs-cli")]
#[command(about = "Connect to Music Assistant and play audio", long_about = None)]
struct Args {
    #[arg(short, long)]
    server: Option<String>,
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

    // Determine server address (either from args or mDNS discovery)
    let server_addr = match args.server {
        Some(addr) => {
            info!("Using specified server: {}", addr);
            addr
        }
        None => {
            info!("No server specified, attempting mDNS discovery...");
            match mdns::discover_sendspin_server() {
                Ok(addr) => addr,
                Err(e) => {
                    error!("Failed to discover Sendspin server: {}", e);
                    error!("Please specify a server with --server <host:port>");
                    std::process::exit(1);
                }
            }
        }
    };

    // Connect
    let ws_url = format!("ws://{}/sendspin", server_addr);
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

    // Use compatibility shim to fix field names for Music Assistant
    let (mut message_rx, mut audio_rx, clock_sync, ws_tx) =
        compat::connect_with_compat(&ws_url, hello).await?;
    info!("Connected!");

    // Send initial state
    let initial_state = Message::ClientState(ClientState {
        player: Some(PlayerState {
            state: PlayerSyncState::Synchronized,
            volume: Some(args.volume),
            muted: Some(false),
        }),
    });
    ws_tx.send_message(initial_state).await?;
    info!("Sent initial client/state");

    // Send initial time sync
    let client_transmitted = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as i64;
    ws_tx
        .send_message(Message::ClientTime(ClientTime { client_transmitted }))
        .await?;

    // Periodic time sync - need to use ProtocolClient::send_message in background task
    // For now, skip periodic sync in background to keep it simple
    // TODO: Add back periodic sync by restructuring to use shared WsSender

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
                        if let Some(player_config) = &stream_start.player {
                            let codec = &player_config.codec;
                            let sample_rate = player_config.sample_rate;
                            let channels = player_config.channels;
                            let bit_depth = player_config.bit_depth;

                            if codec != "pcm" || (bit_depth != 16 && bit_depth != 24) {
                                error!("Unsupported format: {} {}bit", codec, bit_depth);
                                continue;
                            }

                            // New stream: Stop old, setup new, Resume
                            player.stop();
                            std::thread::sleep(Duration::from_millis(5)); // Give time to clear
                            player.resume();

                            audio_format = Some(AudioFormat {
                                codec: Codec::Pcm,
                                sample_rate,
                                channels,
                                bit_depth,
                                codec_header: None,
                            });

                            decoder = None;
                            endian_locked = None;
                            next_play_time = None;

                            info!("Stream: {}Hz {}ch {}bit", sample_rate, channels, bit_depth);

                            // Send playing state to server
                            let state = Message::ClientState(ClientState {
                                player: Some(PlayerState {
                                    state: PlayerSyncState::Synchronized,
                                    volume: Some(args.volume),
                                    muted: Some(false),
                                }),
                            });
                            let _ = ws_tx.send_message(state).await;
                        }
                    }
                    Message::StreamEnd(_end_data) => {
                        info!("← stream/end");

                        // Stop playback on stream/end
                        player.stop();
                        next_play_time = None;

                        // Send synchronized state to server (not playing but ready)
                        let state = Message::ClientState(ClientState {
                            player: Some(PlayerState {
                                state: PlayerSyncState::Synchronized,
                                volume: Some(args.volume),
                                muted: Some(false),
                            }),
                        });
                        let _ = ws_tx.send_message(state).await;
                    }
                    Message::StreamClear(_) => {
                        player.stop();
                        decoder = None;
                        audio_format = None;
                        endian_locked = None;
                        next_play_time = None;

                        // Send synchronized state to server
                        let state = Message::ClientState(ClientState {
                            player: Some(PlayerState {
                                state: PlayerSyncState::Synchronized,
                                volume: Some(args.volume),
                                muted: Some(false),
                            }),
                        });
                        let _ = ws_tx.send_message(state).await;
                    }
                    Message::ServerCommand(command) => {
                        // Check if this is a player command
                        if let Some(player_cmd) = &command.player {
                            match player_cmd.command.as_str() {
                                "pause" | "stop" => {
                                    info!("→ Handling pause/stop command");
                                    player.stop();
                                    // Send synchronized state to server
                                    let state = Message::ClientState(ClientState {
                                        player: Some(PlayerState {
                                            state: PlayerSyncState::Synchronized,
                                            volume: Some(args.volume),
                                            muted: Some(false),
                                        }),
                                    });
                                    let _ = ws_tx.send_message(state).await;
                                }
                                "play" => {
                                    info!("→ Handling play command");
                                    player.resume();
                                    // Send playing state to server
                                    let state = Message::ClientState(ClientState {
                                        player: Some(PlayerState {
                                            state: PlayerSyncState::Synchronized,
                                            volume: Some(args.volume),
                                            muted: Some(false),
                                        }),
                                    });
                                    let _ = ws_tx.send_message(state).await;
                                }
                                "volume" => {
                                    if let Some(vol) = player_cmd.volume {
                                        info!("← Setting volume to {}", vol);
                                        player.set_volume(vol);
                                    }
                                }
                                _ => {
                                    debug!("Unknown command: {}", player_cmd.command);
                                }
                            }
                        }
                    }
                    Message::ServerTime(server_time) => {
                        let t4 = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_micros() as i64;
                        clock_sync.lock().await.update(
                            server_time.client_transmitted,
                            server_time.server_received,
                            server_time.server_transmitted,
                            t4
                        );
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
