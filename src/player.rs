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
    Stop,           // Clear queue and close output immediately
    Resume,         // Allow playback to continue
    SetVolume(u8),  // Set volume 0-100
}

/// Audio Player
pub struct Player {
    audio_queue: Arc<Mutex<VecDeque<AudioBuffer>>>,
    control_tx: mpsc::Sender<PlaybackControl>,
}

impl Player {
    /// Create a new player and spawn the playback thread
    pub fn new(initial_volume: u8) -> Self {
        let audio_queue: Arc<Mutex<VecDeque<AudioBuffer>>> =
            Arc::new(Mutex::new(VecDeque::new()));
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
