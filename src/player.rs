// Audio Player Module
//
// Handles all audio playback logic:
// - Simple FIFO queue for incoming audio buffers
// - Time-synced playback
// - Volume control (software scaling)
// - Stop/Resume commands
// - Native device format output (avoids ALSA resampling)

use crate::audio::AudioOutput;
use crate::error::SendspinError;
use log::{error, info};
use sendspin::audio::AudioFormat;
use sendspin::audio::types::Sample;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

/// Player control commands
#[derive(Debug, Clone)]
pub enum PlaybackControl {
    Stop,          // Clear queue and close output immediately
    Resume,        // Allow playback to continue
    SetVolume(u8), // Set volume 0-100
}

/// A decoded audio buffer scheduled for local playback.
///
/// sendspin's own `AudioBuffer` no longer carries a local play time (0.3.0
/// dropped it in favor of live scheduling via `SyncedPlayer`); this CLI keeps
/// its own hand-rolled queue/output path (for direct-ALSA support), so it
/// tracks `play_at` itself instead.
pub struct QueuedBuffer {
    pub play_at: Instant,
    pub samples: Arc<[Sample]>,
    pub format: AudioFormat,
}

/// Audio Player
pub struct Player {
    audio_queue: Arc<Mutex<VecDeque<QueuedBuffer>>>,
    queue_condvar: Arc<Condvar>,
    control_tx: mpsc::Sender<PlaybackControl>,
    current_volume: Arc<AtomicU8>,
}

impl Player {
    /// Create a new player and spawn the playback thread
    pub fn new(initial_volume: u8, audio_buffer_frames: u32, device: Option<String>) -> Self {
        let audio_queue: Arc<Mutex<VecDeque<QueuedBuffer>>> = Arc::new(Mutex::new(VecDeque::new()));
        let queue_condvar = Arc::new(Condvar::new());
        let current_volume = Arc::new(AtomicU8::new(initial_volume));
        let queue_clone = Arc::clone(&audio_queue);
        let condvar_clone = Arc::clone(&queue_condvar);
        let volume_clone = Arc::clone(&current_volume);

        let (control_tx, control_rx) = mpsc::channel::<PlaybackControl>();

        // Spawn playback thread
        std::thread::spawn(move || {
            if let Err(e) = Self::playback_thread(
                queue_clone,
                condvar_clone,
                control_rx,
                volume_clone,
                audio_buffer_frames,
                device,
            ) {
                error!("Playback thread error: {}", e);
            }
        });

        Player {
            audio_queue,
            queue_condvar,
            control_tx,
            current_volume,
        }
    }

    /// Add an audio buffer to the playback queue
    pub fn enqueue(&self, buffer: QueuedBuffer) {
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
        self.current_volume.store(volume, Ordering::Relaxed);
        let _ = self.control_tx.send(PlaybackControl::SetVolume(volume));
    }

    /// Get current volume (0-100)
    pub fn volume(&self) -> u8 {
        self.current_volume.load(Ordering::Relaxed)
    }

    /// Playback thread - handles audio output
    fn playback_thread(
        queue: Arc<Mutex<VecDeque<QueuedBuffer>>>,
        condvar: Arc<Condvar>,
        control_rx: mpsc::Receiver<PlaybackControl>,
        volume: Arc<AtomicU8>,
        audio_buffer_frames: u32,
        device: Option<String>,
    ) -> Result<(), SendspinError> {
        let mut output: Option<AudioOutput> = None;
        let mut stopped = true; // Start stopped

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
                        volume.store(vol, Ordering::Relaxed);
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
                    match AudioOutput::new(buffer.format.clone(), audio_buffer_frames, &device) {
                        Ok(out) => {
                            info!(
                                "Audio output initialized with volume {}",
                                volume.load(Ordering::Relaxed)
                            );
                            output = Some(out);
                        }
                        Err(e) => {
                            error!("Failed to create output: {}", e);
                            return Err(e);
                        }
                    }
                }

                // Apply volume scaling to samples
                let current_volume = volume.load(Ordering::Relaxed);
                let samples: Arc<[Sample]> = if current_volume < 100 {
                    let volume_factor = current_volume as f32 / 100.0;
                    let scaled_samples: Vec<_> = buffer
                        .samples
                        .iter()
                        .map(|sample| (*sample as f32 * volume_factor) as Sample)
                        .collect();
                    std::sync::Arc::from(scaled_samples.into_boxed_slice())
                } else {
                    buffer.samples
                };

                // Write audio
                if let Some(ref mut out) = output
                    && let Err(e) = out.write(&samples)
                {
                    error!("Output error: {}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sendspin::audio::Codec;
    use std::time::Instant;

    #[test]
    fn test_player_creation() {
        let player = Player::new(75, 0, None);
        assert!(player.control_tx.send(PlaybackControl::Stop).is_ok());
    }

    #[test]
    fn test_enqueue_buffer() {
        let player = Player::new(50, 0, None);

        let format = AudioFormat {
            codec: Codec::Pcm,
            sample_rate: 44100,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        };

        let samples: Vec<Sample> = vec![0; 1024];
        let buffer = QueuedBuffer {
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
        let player = Player::new(50, 0, None);

        let format = AudioFormat {
            codec: Codec::Pcm,
            sample_rate: 44100,
            channels: 2,
            bit_depth: 16,
            codec_header: None,
        };

        // Add multiple buffers
        for _ in 0..5 {
            let samples: Vec<Sample> = vec![0; 1024];
            let buffer = QueuedBuffer {
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
        let player = Player::new(50, 0, None);

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
        let player = Player::new(50, 0, None);

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
