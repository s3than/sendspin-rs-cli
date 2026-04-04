// Native Audio Output
//
// Builds a cpal stream at the device's native format to avoid ALSA resampling.
// On hosts like Asahi Linux the device may report F32 44100Hz while incoming
// audio is 48000Hz; this module handles the rate conversion in the audio callback.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, warn};
use sendspin::audio::{AudioFormat, Sample};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

/// Audio output that uses the device's native format to avoid ALSA resampling.
///
/// On Asahi Linux the device reports F32 44100Hz, but incoming audio may be
/// 48000Hz. This struct builds the cpal stream at the device's native rate
/// and resamples in the audio callback if needed.
pub struct NativeAudioOutput {
    pub sample_tx: mpsc::SyncSender<Arc<[Sample]>>,
    _stream: cpal::Stream,
    _latency_micros: Arc<AtomicU64>,
}

impl NativeAudioOutput {
    pub fn new(
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

                    if need_new
                        && let Ok(rx) = sample_rx.lock()
                        && let Ok(buf) = rx.try_recv()
                    {
                        current_buffer = Some(buf);
                        buffer_pos = 0;
                        resample_pos = 0.0;
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

    pub fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), Box<dyn std::error::Error>> {
        self.sample_tx
            .send(Arc::clone(samples))
            .map_err(|_| "Failed to send samples to audio thread")?;
        Ok(())
    }
}
