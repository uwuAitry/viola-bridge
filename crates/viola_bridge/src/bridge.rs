// SPDX-License-Identifier: GPL-3.0-or-later
//! The `FormatBridge` implementation: a channel-bed source for `orender`.
//!
//! Two input shapes are accepted, chosen by `VIOLA_BRIDGE_MODE`:
//!
//! | mode | behaviour |
//! |---|---|
//! | `auto` (default) | a `RIFF`/`WAVE` byte stream is parsed as WAV; anything else is treated as headerless PCM |
//! | `wav` | always parse a WAVE header (a non-RIFF stream is a fatal error) |
//! | `raw` | never look for a header; the stream is headerless PCM |
//!
//! Headerless PCM takes its layout from the environment:
//! `VIOLA_BRIDGE_CHANNELS` (default `16`), `VIOLA_BRIDGE_RATE` (default `48000`)
//! and `VIOLA_BRIDGE_FORMAT` (`f32` default, or `s16` / `s24` / `s32`).
//!
//! Frames are emitted with **empty metadata**: that is how the renderer
//! recognises a plain channel bed and takes the bed path instead of the object
//! path. Each channel carries an [`RChannelLabel`], which is what gives it a
//! position.

use abi_stable::std_types::{ROption, RSlice, RStr, RString, RVec};
use bridge_api::{
    FormatBridge, RChannelLabel, RChannelPose, RCoordinateFormat, RDecodedFrame, RInputTransport,
    RMetadataFrame, RPushResult, RVbapCartesianDefaults, RVbapTableMode,
};

use crate::pcm::{
    HeaderScan, PcmFormat, SampleFormat, could_be_riff, scan_wav_header, starts_with_riff,
};

/// Maximum sample-frames per emitted [`RDecodedFrame`].
const BLOCK_FRAMES: usize = 2048;

/// Number of leading bytes needed before the mode can be decided.
const SNIFF_BYTES: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Auto,
    Wav,
    Raw,
}

#[derive(Debug, Clone, Copy)]
struct Config {
    mode: Mode,
    channels: u16,
    sample_rate: u32,
    sample_format: SampleFormat,
}

impl Config {
    fn from_env() -> Self {
        let mode = match std::env::var("VIOLA_BRIDGE_MODE")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "wav" => Mode::Wav,
            "raw" => Mode::Raw,
            _ => Mode::Auto,
        };
        let channels = std::env::var("VIOLA_BRIDGE_CHANNELS")
            .ok()
            .and_then(|v| v.trim().parse::<u16>().ok())
            .filter(|c| *c > 0)
            .unwrap_or(16);
        let sample_rate = std::env::var("VIOLA_BRIDGE_RATE")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|r| *r > 0)
            .unwrap_or(48_000);
        let sample_format = std::env::var("VIOLA_BRIDGE_FORMAT")
            .ok()
            .and_then(|v| SampleFormat::from_env(&v))
            .unwrap_or(SampleFormat::F32);
        Self {
            mode,
            channels,
            sample_rate,
            sample_format,
        }
    }

    fn raw_format(&self) -> PcmFormat {
        PcmFormat {
            channels: self.channels,
            sample_rate: self.sample_rate,
            sample_format: self.sample_format,
        }
    }
}

#[derive(Clone, Copy)]
enum State {
    /// Waiting for enough bytes to choose between WAV and headerless PCM.
    Sniffing,
    /// WAV header not yet complete.
    Header,
    /// Streaming PCM.
    Data {
        format: PcmFormat,
        /// `u64::MAX` while the end of the stream is unknown.
        remaining: u64,
    },
}

pub(crate) struct ViolaBridge {
    /// Bytes accumulated across `push_packet` calls but not yet consumed.
    buf: Vec<u8>,
    state: State,
    /// Cached labels for the active channel count.
    labels: Vec<RChannelLabel>,
    config: Config,
    strict: bool,
    frames_emitted: u64,
}

impl ViolaBridge {
    pub(crate) fn new(strict: bool) -> Self {
        let config = Config::from_env();
        let state = if config.mode == Mode::Raw {
            State::Data {
                format: config.raw_format(),
                remaining: u64::MAX,
            }
        } else {
            State::Sniffing
        };
        let labels = labels_for(channel_count_of(&state, &config));
        Self {
            buf: Vec::new(),
            state,
            labels,
            config,
            strict,
            frames_emitted: 0,
        }
    }

    fn reset_state(&mut self) {
        self.buf.clear();
        self.labels.clear();
        self.state = if self.config.mode == Mode::Raw {
            State::Data {
                format: self.config.raw_format(),
                remaining: u64::MAX,
            }
        } else {
            State::Sniffing
        };
        self.labels = labels_for(channel_count_of(&self.state, &self.config));
    }

    fn fail(&mut self, result: &mut RPushResult, message: &str) {
        eprintln!("{message}");
        self.reset_state();
        result.did_reset = true;
        if self.strict {
            result.error_message = RString::from(message);
        }
    }

    /// Move the state machine forward as far as the buffered bytes allow.
    fn advance(&mut self, result: &mut RPushResult) {
        loop {
            match self.state {
                State::Sniffing => {
                    if self.buf.len() < SNIFF_BYTES && could_be_riff(&self.buf) {
                        return; // still ambiguous
                    }
                    if self.config.mode == Mode::Wav || starts_with_riff(&self.buf) {
                        self.state = State::Header;
                    } else {
                        let format = self.config.raw_format();
                        self.labels = labels_for(format.channels as usize);
                        self.state = State::Data {
                            format,
                            remaining: u64::MAX,
                        };
                    }
                }
                State::Header => match scan_wav_header(&self.buf) {
                    HeaderScan::NeedMore => return,
                    HeaderScan::Invalid(reason) => {
                        let message = format!("viola-bridge: invalid WAV: {reason}");
                        self.fail(result, &message);
                        return;
                    }
                    HeaderScan::Found {
                        format,
                        data_offset,
                        data_len,
                    } => {
                        self.labels = labels_for(format.channels as usize);
                        self.buf.drain(0..data_offset);
                        self.state = State::Data {
                            format,
                            remaining: data_len,
                        };
                    }
                },
                State::Data { .. } => break,
            }
        }
        self.drain_pcm(result);
    }

    /// Convert every complete sample-frame currently buffered.
    fn drain_pcm(&mut self, result: &mut RPushResult) {
        let State::Data { format, remaining } = self.state else {
            return;
        };
        let bytes_per_frame = format.bytes_per_frame();
        if bytes_per_frame == 0 {
            return;
        }
        let channels = format.channels as usize;

        let available = if remaining == u64::MAX {
            self.buf.len()
        } else {
            self.buf.len().min(remaining as usize)
        };
        let total_frames = available / bytes_per_frame;
        if total_frames == 0 {
            return;
        }

        let mut cursor = 0usize;
        let mut frames_left = total_frames;
        while frames_left > 0 {
            let n = frames_left.min(BLOCK_FRAMES);
            let mut pcm: RVec<i32> = RVec::with_capacity(n * channels);
            let end = cursor + n * bytes_per_frame;
            let bytes = &self.buf[cursor..end];
            let step = format.sample_format.bytes_per_sample();
            for sample in bytes.chunks_exact(step) {
                pcm.push(format.sample_format.decode(sample));
            }
            result.frames.push(RDecodedFrame {
                sampling_frequency: format.sample_rate,
                sample_count: n as u32,
                channel_count: format.channels as u32,
                pcm,
                channel_labels: RVec::from(self.labels.clone()),
                // Empty metadata == plain channel bed.
                metadata: RVec::<RMetadataFrame>::new(),
                drc_gain: 1.0,
                drc_ramp_duration: 0,
                dialogue_level: ROption::RNone,
                is_new_segment: false,
            });
            cursor = end;
            frames_left -= n;
        }

        self.frames_emitted += total_frames as u64;
        if let State::Data { remaining, .. } = &mut self.state {
            if *remaining != u64::MAX {
                *remaining = remaining.saturating_sub((total_frames * bytes_per_frame) as u64);
            }
        }
        self.buf.drain(0..total_frames * bytes_per_frame);
    }
}

fn channel_count_of(state: &State, config: &Config) -> usize {
    match state {
        State::Data { format, .. } => format.channels as usize,
        _ => config.channels as usize,
    }
}

/// Channel labels for a given channel count.
///
/// Recognised counts use the conventional interleave order; `16` is the 9.1.6
/// set this bridge exists for (the upstream reference bridge stops at 12).
/// Anything else gets the wide-layout prefix plus `Unknown`.
fn labels_for(channels: usize) -> Vec<RChannelLabel> {
    use RChannelLabel::*;

    /// 9.1.6: front, front-wide, side, back, then the three height tiers.
    const WIDE: &[RChannelLabel] = &[
        L, R, C, LFE, Lw, Rw, Ls, Rs, Lb, Rb, Tfl, Tfr, Tsl, Tsr, Tbl, Tbr,
    ];

    let exact: &[RChannelLabel] = match channels {
        1 => &[C],
        2 => &[L, R],
        6 => &[L, R, C, LFE, Ls, Rs],
        8 => &[L, R, C, LFE, Ls, Rs, Lb, Rb],
        10 => &[L, R, C, LFE, Lw, Rw, Ls, Rs, Lb, Rb],
        12 => &[L, R, C, LFE, Ls, Rs, Lb, Rb, Tfl, Tfr, Tbl, Tbr],
        16 => WIDE,
        _ => &[],
    };
    if channels == exact.len() {
        return exact.to_vec();
    }
    (0..channels)
        .map(|i| WIDE.get(i).copied().unwrap_or(Unknown))
        .collect()
}

impl FormatBridge for ViolaBridge {
    fn push_packet(
        &mut self,
        data: RSlice<'_, u8>,
        _transport: RInputTransport,
        _data_type: u8,
    ) -> RPushResult {
        let mut result = RPushResult {
            frames: RVec::new(),
            error_message: RString::new(),
            did_reset: false,
        };
        self.buf.extend_from_slice(data.as_slice());
        self.advance(&mut result);
        result
    }

    fn reset(&mut self) {
        self.reset_state();
    }

    fn is_ready(&self) -> bool {
        self.frames_emitted > 0
    }

    fn has_objects(&self) -> bool {
        // A PCM/WAV stream carries fixed channels only.
        false
    }

    fn configure(&mut self, key: RStr<'_>, _value: RStr<'_>) -> bool {
        // The host always selects a presentation; a single-stream bridge accepts
        // (and ignores) it. Returning false here aborts host startup.
        key.as_str() == "presentation"
    }

    fn coordinate_format(&self) -> RCoordinateFormat {
        RCoordinateFormat::Cartesian
    }

    fn vbap_cartesian_defaults(&self) -> RVbapCartesianDefaults {
        RVbapCartesianDefaults {
            x_size: 62,
            y_size: 62,
            z_size: 15,
            allow_negative_z: false,
        }
    }

    fn preferred_vbap_table_mode(&self) -> RVbapTableMode {
        RVbapTableMode::Cartesian
    }

    fn supported_drc_modes(&self) -> RVec<RString> {
        RVec::new()
    }

    fn set_drc_mode(&mut self, _mode: RStr<'_>) -> bool {
        false
    }

    fn fixed_channel_poses(&self) -> RVec<RChannelPose> {
        // Linear PCM states no angles: every channel takes the renderer's own
        // catalogue pose for its label.
        RVec::new()
    }

    fn source_family(&self) -> RString {
        RString::from("pcm")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav_16ch_pcm16(frames: &[i16]) -> Vec<u8> {
        let mut data = Vec::new();
        for f in frames {
            for _ in 0..16 {
                data.extend_from_slice(&f.to_le_bytes());
            }
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(&48_000u32.to_le_bytes());
        out.extend_from_slice(&(48_000u32 * 16 * 2).to_le_bytes());
        out.extend_from_slice(&(16u16 * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    fn push_all(bridge: &mut ViolaBridge, bytes: &[u8], chunk: usize) -> (u32, usize) {
        let mut frames = 0u32;
        let mut channels = 0usize;
        for part in bytes.chunks(chunk) {
            let r = bridge.push_packet(RSlice::from_slice(part), RInputTransport::Raw, 0);
            assert!(r.error_message.is_empty(), "unexpected error");
            for f in r.frames.iter() {
                frames += f.sample_count;
                channels = f.channel_count as usize;
            }
        }
        (frames, channels)
    }

    #[test]
    fn decodes_16_channel_wav_split_across_pushes() {
        let frames: Vec<i16> = (0..32).map(|i| i as i16).collect();
        let bytes = wav_16ch_pcm16(&frames);
        let mut bridge = ViolaBridge::new(false);
        let (decoded, channels) = push_all(&mut bridge, &bytes, 7);
        assert_eq!(decoded, 32);
        assert_eq!(channels, 16);
        assert!(bridge.is_ready());
        assert!(!bridge.has_objects());
    }

    #[test]
    fn sixteen_channel_labels_are_9_1_6() {
        use RChannelLabel::*;
        assert_eq!(
            labels_for(16),
            vec![L, R, C, LFE, Lw, Rw, Ls, Rs, Lb, Rb, Tfl, Tfr, Tsl, Tsr, Tbl, Tbr]
        );
        // Unrecognised counts degrade instead of panicking.
        assert_eq!(labels_for(3), vec![L, R, C]);
        assert_eq!(labels_for(17).len(), 17);
        assert_eq!(labels_for(17)[16], Unknown);
    }

    #[test]
    fn rejects_non_wav_in_wav_mode() {
        // Mode is read from the environment at construction; default is Auto.
        let mut bridge = ViolaBridge::new(true);
        bridge.config.mode = Mode::Wav;
        bridge.reset_state();
        let r = bridge.push_packet(
            RSlice::from_slice(b"not a wave file at all, but long enough"),
            RInputTransport::Raw,
            0,
        );
        assert!(!r.error_message.is_empty(), "strict mode must report");
    }
}
