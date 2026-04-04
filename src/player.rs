// Audio Player Module
//
// Handles all audio playback logic:
// - Simple FIFO queue for incoming audio buffers
// - Time-synced playback
// - Volume control (software scaling)
// - Stop/Resume commands
// - Native device format output (avoids ALSA resampling)

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{error, info, warn};
use sendspin::audio::{AudioBuffer, AudioFormat, Sample};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

/// Player control commands
#[derive(Debug, Clone)]
pub enum PlaybackControl {
    Stop,          // Clear queue and close output immediately
    Resume,        // Allow playback to continue
    SetVolume(u8), // Set volume 0-100
}

/// Audio output that uses the device's native format to avoid ALSA resampling.
///
/// On Asahi Linux the device reports F32 44100Hz, but incoming audio may be
/// 48000Hz. This struct builds the cpal stream at the device's native rate
/// and resamples in the audio callback if needed.
struct NativeAudioOutput {
    sample_tx: mpsc::SyncSender<Arc<[Sample]>>,
    _stream: cpal::Stream,
    _latency_micros: Arc<AtomicU64>,
}

impl NativeAudioOutput {
    fn new(
        input_format: AudioFormat,
        audio_buffer_frames: u32,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or("No output device available")?;

        let default_config = device.default_output_config()?;
        let device_sample_rate = default_config.sample_rate();
        let device_channels = default_config.channels();
        let device_sample_format = default_config.sample_format();

        info!(
            "Device native: {:?} {}Hz {}ch",
            device_sample_format, device_sample_rate, device_channels
        );
        info!(
            "Input format: {}Hz {}ch {}bit",
            input_format.sample_rate, input_format.channels, input_format.bit_depth
        );

        let input_rate = input_format.sample_rate;
        let input_channels = input_format.channels as u16;
        let needs_resample = input_rate != device_sample_rate as u32;

        if needs_resample {
            info!(
                "Resampling {}Hz -> {}Hz in audio callback",
                input_rate, device_sample_rate
            );
        }

        // Build stream at the device's native config.
        let buffer_size = if audio_buffer_frames == 0 {
            cpal::BufferSize::Default
        } else {
            cpal::BufferSize::Fixed(audio_buffer_frames)
        };
        let config = cpal::StreamConfig {
            channels: device_channels,
            sample_rate: device_sample_rate,
            buffer_size,
        };

        // Bounded channel for backpressure (~10 buffers)
        let (sample_tx, sample_rx) = mpsc::sync_channel::<Arc<[Sample]>>(10);
        let sample_rx = Mutex::new(sample_rx);

        let latency_micros = Arc::new(AtomicU64::new(0));
        let latency_clone = Arc::clone(&latency_micros);

        // State for the audio callback
        let mut current_buffer: Option<Arc<[Sample]>> = None;
        let mut buffer_pos: usize = 0;
        // Resampling state: fractional position in the input buffer
        let device_rate_u32 = device_sample_rate as u32;
        let ratio = if needs_resample {
            input_rate as f64 / device_rate_u32 as f64
        } else {
            1.0
        };
        let mut resample_pos: f64 = 0.0;

        let stream = device.build_output_stream(
            &config,
            move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                // Track latency
                let ts = info.timestamp();
                if let Some(latency) = ts.playback.duration_since(&ts.callback) {
                    latency_clone.store(latency.as_micros() as u64, Ordering::Relaxed);
                }

                for sample_out in data.iter_mut() {
                    // Ensure we have a buffer
                    let need_new = match current_buffer {
                        Some(ref buf) => {
                            if needs_resample {
                                // For resampling, check if fractional pos exceeds buffer
                                let max_pos = buf.len() / input_channels as usize;
                                resample_pos >= max_pos as f64
                            } else {
                                buffer_pos >= buf.len()
                            }
                        }
                        None => true,
                    };

                    if need_new {
                        if let Ok(rx) = sample_rx.lock() {
                            if let Ok(buf) = rx.try_recv() {
                                current_buffer = Some(buf);
                                buffer_pos = 0;
                                resample_pos = 0.0;
                            }
                        }
                    }

                    if let Some(ref buf) = current_buffer {
                        if needs_resample {
                            // Linear interpolation resampling
                            let frames = buf.len() / input_channels as usize;
                            let frame_idx = resample_pos as usize;

                            if frame_idx < frames {
                                // Which output channel are we filling?
                                // cpal interleaves channels: [L, R, L, R, ...]
                                // We track which channel via buffer_pos % device_channels
                                let out_ch = buffer_pos % device_channels as usize;
                                let in_ch = if out_ch < input_channels as usize {
                                    out_ch
                                } else {
                                    0 // downmix: repeat first channel
                                };

                                let idx0 = frame_idx * input_channels as usize + in_ch;
                                let s0 = buf[idx0].to_f32();

                                let s1 = if frame_idx + 1 < frames {
                                    let idx1 = (frame_idx + 1) * input_channels as usize + in_ch;
                                    buf[idx1].to_f32()
                                } else {
                                    s0
                                };

                                let frac = resample_pos - frame_idx as f64;
                                *sample_out = s0 + (s1 - s0) * frac as f32;

                                buffer_pos += 1;
                                // Advance fractional position once per frame (after all channels)
                                if buffer_pos.is_multiple_of(device_channels as usize) {
                                    resample_pos += ratio;
                                }
                            } else {
                                *sample_out = 0.0;
                                buffer_pos += 1;
                            }
                        } else {
                            // No resampling needed - direct copy
                            if buffer_pos < buf.len() {
                                *sample_out = buf[buffer_pos].to_f32();
                                buffer_pos += 1;
                            } else {
                                *sample_out = 0.0;
                            }
                        }
                    } else {
                        *sample_out = 0.0;
                    }
                }
            },
            |err| {
                warn!("Audio stream error: {}", err);
            },
            None,
        )?;

        stream.play()?;

        Ok(NativeAudioOutput {
            sample_tx,
            _stream: stream,
            _latency_micros: latency_micros,
        })
    }

    fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), Box<dyn std::error::Error>> {
        self.sample_tx
            .send(Arc::clone(samples))
            .map_err(|_| "Failed to send samples to audio thread")?;
        Ok(())
    }
}

/// Audio Player
pub struct Player {
    audio_queue: Arc<Mutex<VecDeque<AudioBuffer>>>,
    queue_condvar: Arc<Condvar>,
    control_tx: mpsc::Sender<PlaybackControl>,
}

impl Player {
    /// Create a new player and spawn the playback thread
    pub fn new(initial_volume: u8, audio_buffer_frames: u32) -> Self {
        let audio_queue: Arc<Mutex<VecDeque<AudioBuffer>>> = Arc::new(Mutex::new(VecDeque::new()));
        let queue_condvar = Arc::new(Condvar::new());
        let queue_clone = Arc::clone(&audio_queue);
        let condvar_clone = Arc::clone(&queue_condvar);

        let (control_tx, control_rx) = mpsc::channel::<PlaybackControl>();

        // Spawn playback thread
        std::thread::spawn(move || {
            if let Err(e) = Self::playback_thread(
                queue_clone,
                condvar_clone,
                control_rx,
                initial_volume,
                audio_buffer_frames,
            ) {
                error!("Playback thread error: {}", e);
            }
        });

        Player {
            audio_queue,
            queue_condvar,
            control_tx,
        }
    }

    /// Add an audio buffer to the playback queue
    pub fn enqueue(&self, buffer: AudioBuffer) {
        self.audio_queue.lock().unwrap().push_back(buffer);
        self.queue_condvar.notify_one();
    }

    /// Stop playback and clear the queue
    pub fn stop(&self) {
        let _ = self.control_tx.send(PlaybackControl::Stop);
        self.queue_condvar.notify_one();
    }

    /// Resume playback
    pub fn resume(&self) {
        let _ = self.control_tx.send(PlaybackControl::Resume);
        self.queue_condvar.notify_one();
    }

    /// Set volume (0-100)
    pub fn set_volume(&self, volume: u8) {
        let _ = self.control_tx.send(PlaybackControl::SetVolume(volume));
    }

    /// Playback thread - handles audio output
    fn playback_thread(
        queue: Arc<Mutex<VecDeque<AudioBuffer>>>,
        condvar: Arc<Condvar>,
        control_rx: mpsc::Receiver<PlaybackControl>,
        initial_volume: u8,
        audio_buffer_frames: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut output: Option<NativeAudioOutput> = None;
        let mut stopped = true; // Start stopped
        let mut current_volume: u8 = initial_volume;

        loop {
            // Check for control commands
            while let Ok(cmd) = control_rx.try_recv() {
                match cmd {
                    PlaybackControl::Stop => {
                        info!("→ Playback: STOP");
                        // Clear everything instantly
                        queue.lock().unwrap().clear();
                        output = None; // Drops output, stops audio immediately
                        stopped = true;
                    }
                    PlaybackControl::Resume => {
                        info!("→ Playback: RESUME");
                        stopped = false;
                    }
                    PlaybackControl::SetVolume(vol) => {
                        info!("→ Playback: SET VOLUME {}", vol);
                        current_volume = vol;
                    }
                }
            }

            // If stopped, wait on condvar until woken (by resume/stop/enqueue)
            if stopped {
                let guard = queue.lock().unwrap();
                let _ = condvar
                    .wait_timeout(guard, Duration::from_millis(100))
                    .unwrap();
                continue;
            }

            // Get next buffer, or wait on condvar if queue is empty
            let buffer = {
                let mut guard = queue.lock().unwrap();
                if guard.is_empty() {
                    let (guard_after, _) = condvar
                        .wait_timeout(guard, Duration::from_millis(10))
                        .unwrap();
                    guard = guard_after;
                }
                guard.pop_front()
            };

            if let Some(buffer) = buffer {
                // Time-sync: wait until play_at time
                let now = std::time::Instant::now();
                if buffer.play_at > now {
                    let wait = buffer.play_at - now;
                    if wait < Duration::from_millis(100) {
                        std::thread::sleep(wait);
                    } else {
                        // Too far in future, put back and wait
                        queue.lock().unwrap().push_front(buffer);
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                }

                // Initialize output if needed
                if output.is_none() {
                    match NativeAudioOutput::new(buffer.format.clone(), audio_buffer_frames) {
                        Ok(out) => {
                            info!("Audio output initialized with volume {}", current_volume);
                            output = Some(out);
                        }
                        Err(e) => {
                            error!("Failed to create output: {}", e);
                            return Err(e);
                        }
                    }
                }

                // Apply volume scaling to samples
                let samples = if current_volume < 100 {
                    let volume_factor = current_volume as f32 / 100.0;
                    let scaled_samples: Vec<_> = buffer
                        .samples
                        .iter()
                        .map(|sample| Sample((sample.0 as f32 * volume_factor) as i32))
                        .collect();
                    std::sync::Arc::from(scaled_samples.into_boxed_slice())
                } else {
                    buffer.samples
                };

                // Write audio
                if let Some(ref mut out) = output {
                    if let Err(e) = out.write(&samples) {
                        error!("Output error: {}", e);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendspin::audio::{AudioFormat, Codec, Sample};
    use std::time::Instant;

    #[test]
    fn test_player_creation() {
        let player = Player::new(75, 0);
        assert!(player.control_tx.send(PlaybackControl::Stop).is_ok());
    }

    #[test]
    fn test_enqueue_buffer() {
        let player = Player::new(50, 0);

        let format = AudioFormat {
            codec: Codec::Pcm,
            sample_rate: 44100,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        };

        let samples = vec![Sample(0); 1024];
        let buffer = AudioBuffer {
            timestamp: 0,
            format,
            samples: Arc::from(samples.into_boxed_slice()),
            play_at: Instant::now(),
        };

        player.enqueue(buffer);

        // Verify buffer was added to queue
        let queue_size = player.audio_queue.lock().unwrap().len();
        assert_eq!(queue_size, 1);
    }

    #[test]
    fn test_stop_clears_queue() {
        let player = Player::new(50, 0);

        let format = AudioFormat {
            codec: Codec::Pcm,
            sample_rate: 44100,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        };

        // Add multiple buffers
        for _ in 0..5 {
            let samples = vec![Sample(0); 1024];
            let buffer = AudioBuffer {
                timestamp: 0,
                format: format.clone(),
                samples: Arc::from(samples.into_boxed_slice()),
                play_at: Instant::now(),
            };
            player.enqueue(buffer);
        }

        // Stop should clear queue
        player.stop();

        // Give the playback thread time to process the stop command
        std::thread::sleep(Duration::from_millis(50));

        let queue_size = player.audio_queue.lock().unwrap().len();
        assert_eq!(queue_size, 0);
    }

    #[test]
    fn test_control_commands() {
        let player = Player::new(50, 0);

        // Test all control commands send successfully
        assert!(player.control_tx.send(PlaybackControl::Stop).is_ok());
        assert!(player.control_tx.send(PlaybackControl::Resume).is_ok());
        assert!(
            player
                .control_tx
                .send(PlaybackControl::SetVolume(80))
                .is_ok()
        );
    }

    #[test]
    fn test_volume_control() {
        let player = Player::new(50, 0);

        // Test volume bounds
        player.set_volume(0);
        player.set_volume(50);
        player.set_volume(100);

        // Give thread time to process
        std::thread::sleep(Duration::from_millis(10));
    }

    #[test]
    fn test_playback_control_debug() {
        // Test Debug trait implementation
        let stop = PlaybackControl::Stop;
        let resume = PlaybackControl::Resume;
        let volume = PlaybackControl::SetVolume(75);

        assert_eq!(format!("{:?}", stop), "Stop");
        assert_eq!(format!("{:?}", resume), "Resume");
        assert_eq!(format!("{:?}", volume), "SetVolume(75)");
    }

    #[test]
    fn test_playback_control_clone() {
        // Test Clone trait implementation
        let original = PlaybackControl::SetVolume(50);
        let cloned = original.clone();

        assert!(matches!(cloned, PlaybackControl::SetVolume(50)));
    }
}
