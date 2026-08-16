// Native Audio Output
//
// Builds a cpal stream at the device's native format to avoid ALSA resampling.
// On hosts like Asahi Linux the device may report F32 44100Hz while incoming
// audio is 48000Hz; this module handles the rate conversion in the audio callback.

use cpal::Sample as _;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use log::{info, warn};
use sendspin::audio::AudioFormat;
use sendspin::audio::types::Sample;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};

use crate::error::SendspinError;

#[cfg(target_os = "linux")]
use crate::alsa_output::AlsaAudioOutput;

/// Unified audio output that dispatches between cpal (default) and direct ALSA.
pub enum AudioOutput {
    Cpal(NativeAudioOutput),
    #[cfg(target_os = "linux")]
    Alsa(AlsaAudioOutput),
}

impl AudioOutput {
    /// Create a new audio output. When `device` is Some on Linux, uses direct ALSA;
    /// otherwise uses cpal (PipeWire/default host).
    pub fn new(
        input_format: AudioFormat,
        audio_buffer_frames: u32,
        device: &Option<String>,
    ) -> Result<Self, SendspinError> {
        #[cfg(target_os = "linux")]
        if let Some(alsa_device) = device {
            return Ok(AudioOutput::Alsa(AlsaAudioOutput::new(
                alsa_device,
                &input_format,
            )?));
        }

        #[cfg(not(target_os = "linux"))]
        let _ = device; // suppress unused warning

        Ok(AudioOutput::Cpal(NativeAudioOutput::new(
            input_format,
            audio_buffer_frames,
        )?))
    }

    pub fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), SendspinError> {
        match self {
            AudioOutput::Cpal(out) => out.write(samples),
            #[cfg(target_os = "linux")]
            AudioOutput::Alsa(out) => out.write(samples),
        }
    }
}

/// Audio output that uses cpal at the device's native format (default host).
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
    pub fn new(input_format: AudioFormat, audio_buffer_frames: u32) -> Result<Self, SendspinError> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or(SendspinError::NoOutputDevice)?;

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

        // Shared callback state — produces the next sample as f32.
        // Moved into a closure so the same logic works for all output formats.
        let next_sample = move |current_buffer: &mut Option<Arc<[Sample]>>,
                                buffer_pos: &mut usize,
                                resample_pos: &mut f64|
              -> f32 {
            let need_new = match current_buffer {
                Some(buf) => {
                    if needs_resample {
                        let max_pos = buf.len() / input_channels as usize;
                        *resample_pos >= max_pos as f64
                    } else {
                        *buffer_pos >= buf.len()
                    }
                }
                None => true,
            };

            if need_new
                && let Ok(rx) = sample_rx.lock()
                && let Ok(buf) = rx.try_recv()
            {
                *current_buffer = Some(buf);
                *buffer_pos = 0;
                *resample_pos = 0.0;
            }

            if let Some(buf) = current_buffer {
                if needs_resample {
                    let frames = buf.len() / input_channels as usize;
                    let frame_idx = *resample_pos as usize;

                    if frame_idx < frames {
                        let out_ch = *buffer_pos % device_channels as usize;
                        let in_ch = if out_ch < input_channels as usize {
                            out_ch
                        } else {
                            0
                        };

                        let idx0 = frame_idx * input_channels as usize + in_ch;
                        let s0 = f32::from_sample(buf[idx0]);

                        let s1 = if frame_idx + 1 < frames {
                            let idx1 = (frame_idx + 1) * input_channels as usize + in_ch;
                            f32::from_sample(buf[idx1])
                        } else {
                            s0
                        };

                        let frac = *resample_pos - frame_idx as f64;
                        let val = s0 + (s1 - s0) * frac as f32;

                        *buffer_pos += 1;
                        if (*buffer_pos).is_multiple_of(device_channels as usize) {
                            *resample_pos += ratio;
                        }
                        val
                    } else {
                        *buffer_pos += 1;
                        0.0
                    }
                } else if *buffer_pos < buf.len() {
                    let val = f32::from_sample(buf[*buffer_pos]);
                    *buffer_pos += 1;
                    val
                } else {
                    0.0
                }
            } else {
                0.0
            }
        };

        // Build the stream using the device's native sample format.
        let stream = match device_sample_format {
            cpal::SampleFormat::I16 => device.build_output_stream(
                &config,
                move |data: &mut [i16], info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    if let Some(latency) = ts.playback.duration_since(&ts.callback) {
                        latency_clone.store(latency.as_micros() as u64, Ordering::Relaxed);
                    }
                    for sample_out in data.iter_mut() {
                        let val =
                            next_sample(&mut current_buffer, &mut buffer_pos, &mut resample_pos);
                        *sample_out = (val * i16::MAX as f32) as i16;
                    }
                },
                |err| warn!("Audio stream error: {}", err),
                None,
            )?,
            cpal::SampleFormat::I32 => device.build_output_stream(
                &config,
                move |data: &mut [i32], info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    if let Some(latency) = ts.playback.duration_since(&ts.callback) {
                        latency_clone.store(latency.as_micros() as u64, Ordering::Relaxed);
                    }
                    for sample_out in data.iter_mut() {
                        let val =
                            next_sample(&mut current_buffer, &mut buffer_pos, &mut resample_pos);
                        *sample_out = (val as f64 * i32::MAX as f64) as i32;
                    }
                },
                |err| warn!("Audio stream error: {}", err),
                None,
            )?,
            _ => device.build_output_stream(
                &config,
                move |data: &mut [f32], info: &cpal::OutputCallbackInfo| {
                    let ts = info.timestamp();
                    if let Some(latency) = ts.playback.duration_since(&ts.callback) {
                        latency_clone.store(latency.as_micros() as u64, Ordering::Relaxed);
                    }
                    for sample_out in data.iter_mut() {
                        *sample_out =
                            next_sample(&mut current_buffer, &mut buffer_pos, &mut resample_pos);
                    }
                },
                |err| warn!("Audio stream error: {}", err),
                None,
            )?,
        };

        stream.play()?;

        Ok(NativeAudioOutput {
            sample_tx,
            _stream: stream,
            _latency_micros: latency_micros,
        })
    }

    pub fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), SendspinError> {
        self.sample_tx
            .send(Arc::clone(samples))
            .map_err(|_| SendspinError::Audio("Failed to send samples to audio thread".into()))?;
        Ok(())
    }
}
