// Direct ALSA Audio Output (Linux-only)
//
// Opens an ALSA device string directly (e.g. "plughw:0,0", "default:CARD=MOMENTUM4")
// bypassing PipeWire. This is needed for headless/GDM scenarios where PipeWire
// isn't running.

#![cfg(target_os = "linux")]

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use log::info;
use sendspin::audio::{AudioFormat, Sample};
use std::sync::Arc;

use crate::error::SendspinError;

pub struct AlsaAudioOutput {
    pcm: PCM,
    channels: u32,
    use_i32: bool,
}

impl AlsaAudioOutput {
    pub fn new(device: &str, input_format: &AudioFormat) -> Result<Self, SendspinError> {
        let pcm = PCM::new(device, Direction::Playback, false)
            .map_err(|e| SendspinError::DeviceNotFound(format!("{}: {}", device, e)))?;

        // Preferred format order: match input bit depth, then fall back
        let formats_to_try = match input_format.bit_depth {
            16 => vec![(Format::s16(), false)],
            24 => vec![(Format::s32(), true), (Format::s16(), false)],
            _ => {
                return Err(SendspinError::Audio(format!(
                    "unsupported bit depth: {}",
                    input_format.bit_depth
                )));
            }
        };

        let channels = input_format.channels as u32;

        // Try each format until one works
        let mut use_i32 = false;
        let mut chosen_format = Format::s16();
        let mut installed = false;

        for (format, is_i32) in &formats_to_try {
            let hwp = HwParams::any(&pcm)
                .map_err(|e| SendspinError::Audio(format!("ALSA hw_params_any failed: {}", e)))?;

            hwp.set_access(Access::RWInterleaved)
                .map_err(|e| SendspinError::Audio(format!("ALSA set_access failed: {}", e)))?;

            if hwp.set_format(*format).is_err() {
                info!(
                    "ALSA device '{}': {:?} not supported, trying next",
                    device, format
                );
                continue;
            }

            hwp.set_channels(channels)
                .map_err(|e| SendspinError::Audio(format!("ALSA set_channels failed: {}", e)))?;

            hwp.set_rate(input_format.sample_rate, ValueOr::Nearest)
                .map_err(|e| SendspinError::Audio(format!("ALSA set_rate failed: {}", e)))?;

            // Reasonable buffer/period sizes for low-latency playback
            hwp.set_buffer_size_near(8192).map_err(|e| {
                SendspinError::Audio(format!("ALSA set_buffer_size_near failed: {}", e))
            })?;

            hwp.set_period_size_near(2048, ValueOr::Nearest)
                .map_err(|e| {
                    SendspinError::Audio(format!("ALSA set_period_size_near failed: {}", e))
                })?;

            if let Err(e) = pcm.hw_params(&hwp) {
                info!(
                    "ALSA device '{}': hw_params install failed for {:?}: {}",
                    device, format, e
                );
                continue;
            }

            chosen_format = *format;
            use_i32 = *is_i32;
            installed = true;
            break;
        }

        if !installed {
            return Err(SendspinError::Audio(format!(
                "ALSA device '{}': no supported format found for {}bit input",
                device, input_format.bit_depth
            )));
        }

        {
            let hwp = pcm.hw_params_current().map_err(|e| {
                SendspinError::Audio(format!("ALSA hw_params_current failed: {}", e))
            })?;

            let actual_rate = hwp
                .get_rate()
                .map_err(|e| SendspinError::Audio(format!("ALSA get_rate failed: {}", e)))?;

            let actual_buffer = hwp
                .get_buffer_size()
                .map_err(|e| SendspinError::Audio(format!("ALSA get_buffer_size failed: {}", e)))?;

            let actual_period = hwp
                .get_period_size()
                .map_err(|e| SendspinError::Audio(format!("ALSA get_period_size failed: {}", e)))?;

            info!(
                "ALSA device '{}': {:?} {}Hz {}ch (buffer={} period={})",
                device, chosen_format, actual_rate, channels, actual_buffer, actual_period
            );
        }

        pcm.prepare()
            .map_err(|e| SendspinError::Audio(format!("ALSA prepare failed: {}", e)))?;

        Ok(AlsaAudioOutput {
            pcm,
            channels,
            use_i32,
        })
    }

    pub fn write(&mut self, samples: &Arc<[Sample]>) -> Result<(), SendspinError> {
        if self.use_i32 {
            let io = self
                .pcm
                .io_i32()
                .map_err(|e| SendspinError::Audio(format!("ALSA io_i32 failed: {}", e)))?;
            // Sample.0 is 24-bit in lower bits — shift left 8 to fill S32
            let buf: Vec<i32> = samples.iter().map(|s| s.0 << 8).collect();
            self.write_all(&io, &buf)?;
        } else {
            let io = self
                .pcm
                .io_i16()
                .map_err(|e| SendspinError::Audio(format!("ALSA io_i16 failed: {}", e)))?;
            // Sample.0 is 24-bit in lower bits — shift right 8 to fit S16
            let buf: Vec<i16> = samples.iter().map(|s| s.to_i16()).collect();
            self.write_all(&io, &buf)?;
        }
        Ok(())
    }

    /// Write all frames, handling ALSA underruns (EPIPE) by recovering and retrying.
    fn write_all<S: Copy>(&self, io: &alsa::pcm::IO<S>, buf: &[S]) -> Result<(), SendspinError> {
        let frames = buf.len() / self.channels as usize;
        let mut written = 0;

        while written < frames {
            let remaining = &buf[written * self.channels as usize..];
            match io.writei(remaining) {
                Ok(n) => written += n,
                Err(e) => {
                    // Try to recover from underrun (EPIPE) or suspend
                    self.pcm.recover(e.errno(), true).map_err(|re| {
                        SendspinError::Audio(format!(
                            "ALSA write failed ({}), recovery failed: {}",
                            e, re
                        ))
                    })?;
                }
            }
        }
        Ok(())
    }
}
