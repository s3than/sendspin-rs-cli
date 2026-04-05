// Sendspin Rust CLI Player - Full Protocol Implementation
//
// Design principles:
// 1. Audio IN → Decode → Simple Queue (VecDeque)
// 2. Audio OUT → Time-synced playback from queue
// 3. Stop → Clear queue + drop output (instant)
// 4. Skip → Stop old + Start new (clean transition)
// 5. All output is time-synced to play_at timestamps

pub mod audio;
pub mod config;
pub mod error;
pub mod mdns;
pub mod player;

use clap::Parser;
use log::{debug, error, info, warn};
use player::Player;
use sendspin::audio::decode::{Decoder, PcmDecoder, PcmEndian};
use sendspin::audio::{AudioBuffer, AudioFormat, Codec};
use sendspin::protocol::client::{ProtocolClient, WsSender};
use sendspin::protocol::messages::{
    AudioFormatSpec, ClientHello, ClientState, DeviceInfo, GroupUpdate, Message, PlayerState,
    PlayerSyncState, PlayerV1Support, ServerState,
};
use std::time::{Duration, Instant};

use crate::error::SendspinError;

async fn send_player_state(ws_tx: &WsSender, volume: u8, muted: bool) {
    let state = Message::ClientState(ClientState {
        player: Some(PlayerState {
            state: PlayerSyncState::Synchronized,
            volume: Some(volume),
            muted: Some(muted),
        }),
    });
    let _ = ws_tx.send_message(state).await;
}

#[derive(Parser, Debug)]
#[command(name = "sendspin-rs-cli")]
#[command(about = "Connect to Music Assistant and play audio", long_about = None)]
#[command(version)]
struct Args {
    #[arg(short, long)]
    server: Option<String>,
    #[arg(short, long, default_value = "Sendspin-RS Player")]
    name: String,
    #[arg(long)]
    client_id: Option<String>,
    #[arg(short, long, value_parser = clap::value_parser!(u8).range(0..=100))]
    volume: Option<u8>,
    /// Ignore saved volume and use default (30) or the value from --volume
    #[arg(long)]
    reset_volume: bool,
    #[arg(short, long, default_value = "20")]
    buffer: u64,
    /// Audio device buffer size in frames (0 = system default, try 4096 on Asahi Linux)
    #[arg(long, default_value = "0")]
    audio_buffer: u32,
}

pub async fn run() -> Result<(), SendspinError> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let client_id = args
        .client_id
        .clone()
        .unwrap_or_else(|| format!("sendspin-rs-{}", uuid::Uuid::new_v4()));

    info!("Client ID: {}", client_id);

    // Resolve effective volume: CLI arg > saved config > default 30
    let saved_config = config::AppConfig::load();
    let effective_volume = if args.reset_volume {
        args.volume.unwrap_or(30)
    } else {
        args.volume.or(saved_config.player.volume).unwrap_or(30)
    };
    info!("Initial volume: {}", effective_volume);

    // Create player with resolved volume (persists across reconnects)
    let player = Player::new(effective_volume, args.audio_buffer);
    let buffer_ms = args.buffer;

    let mut reconnect_delay = Duration::from_secs(2);
    let max_reconnect_delay = Duration::from_secs(30);

    loop {
        // Resolve server address (re-discover via mDNS on each attempt if needed)
        let server_addr = match &args.server {
            Some(addr) => {
                info!("Using specified server: {}", addr);
                addr.clone()
            }
            None => {
                info!("Attempting mDNS discovery...");
                match mdns::discover_sendspin_server() {
                    Ok(addr) => addr,
                    Err(e) => {
                        warn!(
                            "mDNS discovery failed: {}. Retrying in {}s...",
                            e,
                            reconnect_delay.as_secs()
                        );
                        tokio::time::sleep(reconnect_delay).await;
                        reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                        continue;
                    }
                }
            }
        };

        let ws_url = format!("ws://{}/sendspin", server_addr);
        info!("Connecting to {}...", ws_url);

        let hello = ClientHello {
            client_id: client_id.clone(),
            name: args.name.clone(),
            version: 1,
            supported_roles: vec!["player@v1".to_string(), "controller@v1".to_string()],
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
                    AudioFormatSpec {
                        codec: "pcm".to_string(),
                        channels: 2,
                        sample_rate: 44100,
                        bit_depth: 24,
                    },
                    AudioFormatSpec {
                        codec: "pcm".to_string(),
                        channels: 2,
                        sample_rate: 44100,
                        bit_depth: 16,
                    },
                ],
                buffer_capacity: 1048576,
                supported_commands: vec!["volume".to_string(), "mute".to_string()],
            }),
            artwork_v1_support: None,
            visualizer_v1_support: None,
        };

        // Connect to server
        let (mut message_rx, mut audio_rx, clock_sync, ws_tx, _guard) =
            match ProtocolClient::connect(&ws_url, hello).await {
                Ok(client) => client.split(),
                Err(e) => {
                    warn!(
                        "Connection failed: {}. Retrying in {}s...",
                        e,
                        reconnect_delay.as_secs()
                    );
                    tokio::time::sleep(reconnect_delay).await;
                    reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
                    continue;
                }
            };

        // Connected successfully — reset backoff
        reconnect_delay = Duration::from_secs(2);
        info!("Connected!");

        // Send initial state
        send_player_state(&ws_tx, player.volume(), false).await;
        info!("Sent initial client/state");

        info!("Waiting for stream to start...");

        // Reset stream state for this connection
        let mut decoder: Option<PcmDecoder> = None;
        let mut audio_format: Option<AudioFormat> = None;
        let mut endian_locked: Option<PcmEndian> = None;
        let mut next_play_time: Option<Instant> = None;

        // Message handling loop
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, shutting down...");
                    player.stop();
                    let vol = player.volume();
                    config::save_volume(vol);
                    info!("Saved volume {} to config", vol);
                    std::process::exit(0);
                }
                Some(msg) = message_rx.recv() => {
                    match &msg {
                        Message::StreamStart(_) => info!("← SERVER: stream/start"),
                        Message::StreamEnd(_) => info!("← SERVER: stream/end"),
                        Message::StreamClear(_) => info!("← SERVER: stream/clear"),
                        Message::ServerCommand(cmd) => info!("← SERVER: command {:?}", cmd),
                        Message::ServerState(_) => debug!("← SERVER: server/state"),
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
                                std::thread::sleep(Duration::from_millis(5));
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
                                send_player_state(&ws_tx, player.volume(), false).await;
                            }
                        }
                        Message::StreamEnd(_end_data) => {
                            info!("← stream/end");

                            player.stop();
                            next_play_time = None;

                            send_player_state(&ws_tx, player.volume(), false).await;
                        }
                        Message::StreamClear(_) => {
                            player.stop();
                            decoder = None;
                            audio_format = None;
                            endian_locked = None;
                            next_play_time = None;

                            send_player_state(&ws_tx, player.volume(), false).await;
                        }
                        Message::ServerCommand(command) => {
                            if let Some(player_cmd) = &command.player {
                                match player_cmd.command.as_str() {
                                    "pause" | "stop" => {
                                        info!("→ Handling pause/stop command");
                                        player.stop();
                                        send_player_state(&ws_tx, player.volume(), false).await;
                                    }
                                    "play" => {
                                        info!("→ Handling play command");
                                        player.resume();
                                        send_player_state(&ws_tx, player.volume(), false).await;
                                    }
                                    "volume" => {
                                        if let Some(vol) = player_cmd.volume {
                                            info!("← Setting volume to {}", vol);
                                            player.set_volume(vol);
                                            tokio::task::spawn_blocking(move || {
                                                config::save_volume(vol);
                                            });
                                        }
                                    }
                                    _ => {
                                        debug!("Unknown command: {}", player_cmd.command);
                                    }
                                }
                            }
                        }
                        Message::ServerState(ServerState { metadata, controller }) => {
                            if let Some(meta) = metadata {
                                let title = meta.title.as_deref().unwrap_or("Unknown");
                                let artist = meta.artist.as_deref().unwrap_or("Unknown");
                                let album = meta.album.as_deref().unwrap_or("Unknown");
                                info!("Now playing: {} - {} [{}]", artist, title, album);
                                if let Some(progress) = &meta.progress {
                                    let pos_s = progress.track_progress / 1000;
                                    let dur_s = progress.track_duration / 1000;
                                    if dur_s > 0 {
                                        debug!("  Progress: {}:{:02} / {}:{:02}", pos_s / 60, pos_s % 60, dur_s / 60, dur_s % 60);
                                    }
                                }
                            }
                            if let Some(ctrl) = controller {
                                debug!("Controller state: volume={}, muted={}, commands={}", ctrl.volume, ctrl.muted, ctrl.supported_commands.join(", "));
                            }
                        }
                        Message::GroupUpdate(GroupUpdate { playback_state, group_id, group_name }) => {
                            let state = playback_state.map(|s| format!("{:?}", s)).unwrap_or_else(|| "unknown".to_string());
                            let name = group_name.as_deref().unwrap_or("unnamed");
                            let id = group_id.as_deref().unwrap_or("?");
                            info!("Group update: {} — state: {} ({})", name, state, id);
                        }
                        _ => {}
                    }
                }

                Some(chunk) = audio_rx.recv() => {
                    if let Some(ref fmt) = audio_format
                        && endian_locked.is_none() {
                            endian_locked = Some(PcmEndian::Little);
                            decoder = Some(PcmDecoder::with_endian(fmt.bit_depth, PcmEndian::Little));
                        }

                    if let (Some(dec), Some(fmt)) = (&decoder, &audio_format)
                        && let Ok(samples) = dec.decode(&chunk.data) {
                            let frames = samples.len() / fmt.channels as usize;
                            let duration = Duration::from_micros(
                                (frames as u64 * 1_000_000) / fmt.sample_rate as u64
                            );

                            let sync = clock_sync.lock();
                            let play_at = if let Some(instant) = sync.server_to_local_instant(chunk.timestamp) {
                                instant
                            } else {
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

                            player.enqueue(buffer);
                        }
                }

                else => break,
            }
        }

        // Disconnected — stop playback and retry
        player.stop();
        info!(
            "Disconnected from server. Reconnecting in {}s...",
            reconnect_delay.as_secs()
        );
        tokio::time::sleep(reconnect_delay).await;
        reconnect_delay = (reconnect_delay * 2).min(max_reconnect_delay);
    }
}
