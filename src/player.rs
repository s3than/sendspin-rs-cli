// Audio Player Module
//
// Handles all audio playback logic:
// - Simple FIFO queue for incoming audio buffers
// - Time-synced playback
// - Volume control (software scaling)
// - Stop/Resume commands

use log::{error, info};
use sendspin::audio::{AudioBuffer, AudioOutput, CpalOutput, Sample};
use std::collections::VecDeque;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

/// Player control commands
#[derive(Debug, Clone)]
pub enum PlaybackControl {
    Stop,          // Clear queue and close output immediately
    Resume,        // Allow playback to continue
    SetVolume(u8), // Set volume 0-100
}

/// Audio Player
pub struct Player {
    audio_queue: Arc<Mutex<VecDeque<AudioBuffer>>>,
    control_tx: mpsc::Sender<PlaybackControl>,
}

impl Player {
    /// Create a new player and spawn the playback thread
    pub fn new(initial_volume: u8) -> Self {
        let audio_queue: Arc<Mutex<VecDeque<AudioBuffer>>> = Arc::new(Mutex::new(VecDeque::new()));
        let queue_clone = Arc::clone(&audio_queue);

        let (control_tx, control_rx) = mpsc::channel::<PlaybackControl>();

        // Spawn playback thread
        std::thread::spawn(move || {
            if let Err(e) = Self::playback_thread(queue_clone, control_rx, initial_volume) {
                error!("Playback thread error: {}", e);
            }
        });

        Player {
            audio_queue,
            control_tx,
        }
    }

    /// Add an audio buffer to the playback queue
    pub fn enqueue(&self, buffer: AudioBuffer) {
        self.audio_queue.lock().unwrap().push_back(buffer);
    }

    /// Stop playback and clear the queue
    pub fn stop(&self) {
        let _ = self.control_tx.send(PlaybackControl::Stop);
    }

    /// Resume playback
    pub fn resume(&self) {
        let _ = self.control_tx.send(PlaybackControl::Resume);
    }

    /// Set volume (0-100)
    pub fn set_volume(&self, volume: u8) {
        let _ = self.control_tx.send(PlaybackControl::SetVolume(volume));
    }

    /// Playback thread - handles audio output
    fn playback_thread(
        queue: Arc<Mutex<VecDeque<AudioBuffer>>>,
        control_rx: mpsc::Receiver<PlaybackControl>,
        initial_volume: u8,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut output: Option<CpalOutput> = None;
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

            // If stopped, don't play anything
            if stopped {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            // Get next buffer
            let buffer = queue.lock().unwrap().pop_front();

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
                    match CpalOutput::new(buffer.format.clone()) {
                        Ok(out) => {
                            info!("Audio output initialized with volume {}", current_volume);
                            output = Some(out);
                        }
                        Err(e) => {
                            error!("Failed to create output: {}", e);
                            return Err(e.into());
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
            } else {
                // Queue empty
                std::thread::sleep(Duration::from_micros(500));
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
        let player = Player::new(75);
        assert!(player.control_tx.send(PlaybackControl::Stop).is_ok());
    }

    #[test]
    fn test_enqueue_buffer() {
        let player = Player::new(50);

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
        let player = Player::new(50);

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
        let player = Player::new(50);

        // Test all control commands send successfully
        assert!(player.control_tx.send(PlaybackControl::Stop).is_ok());
        assert!(player.control_tx.send(PlaybackControl::Resume).is_ok());
        assert!(player
            .control_tx
            .send(PlaybackControl::SetVolume(80))
            .is_ok());
    }

    #[test]
    fn test_volume_control() {
        let player = Player::new(50);

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
