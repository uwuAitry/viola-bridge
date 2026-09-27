// SPDX-License-Identifier: GPL-3.0-or-later
//! Sample encodings and a streaming RIFF/WAVE header scanner.
//!
//! The renderer consumes PCM as interleaved `i32` samples scaled to 24-bit full
//! scale (`2^23 - 1`). Every encoding this bridge accepts is converted to that
//! convention before it leaves the crate.
//!
//! The scanner is written to run over an accumulating buffer: it never assumes
//! the whole header has arrived, so it works on a live stream (a named pipe)
//! exactly as it does on a file.

/// Full-scale magnitude of the renderer's 24-bit-in-`i32` PCM convention.
const PCM_FULL_SCALE: f32 = 8_388_607.0; // 2^23 - 1

/// One sample encoding accepted from the input stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleFormat {
    PcmI16,
    PcmI24,
    PcmI32,
    F32,
}

impl SampleFormat {
    /// Parse the `VIOLA_BRIDGE_FORMAT` spelling (used for headerless streams).
    pub(crate) fn from_env(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "f32" | "float" => Some(Self::F32),
            "s16" | "i16" => Some(Self::PcmI16),
            "s24" | "i24" => Some(Self::PcmI24),
            "s32" | "i32" => Some(Self::PcmI32),
            _ => None,
        }
    }

    pub(crate) fn bytes_per_sample(self) -> usize {
        match self {
            SampleFormat::PcmI16 => 2,
            SampleFormat::PcmI24 => 3,
            SampleFormat::PcmI32 | SampleFormat::F32 => 4,
        }
    }

    /// Convert one little-endian sample to the renderer's 24-bit-scaled `i32`.
    #[inline]
    pub(crate) fn decode(self, bytes: &[u8]) -> i32 {
        match self {
            SampleFormat::PcmI16 => (i16::from_le_bytes([bytes[0], bytes[1]]) as i32) << 8,
            SampleFormat::PcmI24 => {
                let raw = (bytes[0] as i32) | ((bytes[1] as i32) << 8) | ((bytes[2] as i32) << 16);
                (raw << 8) >> 8 // sign-extend from bit 23
            }
            SampleFormat::PcmI32 => {
                i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) >> 8
            }
            SampleFormat::F32 => {
                let v = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                if !v.is_finite() {
                    0
                } else {
                    (v.clamp(-1.0, 1.0) * PCM_FULL_SCALE) as i32
                }
            }
        }
    }
}

/// Interleaved PCM layout of the stream.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PcmFormat {
    pub(crate) channels: u16,
    pub(crate) sample_rate: u32,
    pub(crate) sample_format: SampleFormat,
}

impl PcmFormat {
    pub(crate) fn bytes_per_frame(&self) -> usize {
        self.sample_format.bytes_per_sample() * self.channels as usize
    }
}

/// Outcome of one header-scan attempt.
pub(crate) enum HeaderScan {
    /// Not enough buffered bytes yet.
    NeedMore,
    /// The bytes are not a WAVE stream this bridge understands.
    Invalid(String),
    /// Header parsed; PCM starts at `data_offset`.
    Found {
        format: PcmFormat,
        data_offset: usize,
        /// Declared `data` chunk size in bytes. [`u64::MAX`] means "until the
        /// input ends", which is what a streaming producer writes.
        data_len: u64,
    },
}

/// `true` once at least the four RIFF magic bytes are buffered.
pub(crate) fn starts_with_riff(buf: &[u8]) -> bool {
    buf.len() >= 4 && &buf[0..4] == b"RIFF"
}

/// `true` when the buffered prefix is still consistent with a RIFF stream (so a
/// caller that has not yet seen 12 bytes should keep waiting before deciding).
pub(crate) fn could_be_riff(buf: &[u8]) -> bool {
    let magic = b"RIFF";
    let n = buf.len().min(4);
    buf[..n] == magic[..n]
}

/// Scan `buf` for a complete WAVE header, skipping chunks we do not use.
pub(crate) fn scan_wav_header(buf: &[u8]) -> HeaderScan {
    if buf.len() < 12 {
        return HeaderScan::NeedMore;
    }
    if &buf[0..4] != b"RIFF" {
        return HeaderScan::Invalid("missing RIFF magic".into());
    }
    if &buf[8..12] != b"WAVE" {
        return HeaderScan::Invalid("RIFF form is not WAVE".into());
    }

    let mut format: Option<PcmFormat> = None;
    let mut pos = 12usize;
    loop {
        if pos.checked_add(8).is_none_or(|end| end > buf.len()) {
            return HeaderScan::NeedMore;
        }
        let id = &buf[pos..pos + 4];
        let size = u32::from_le_bytes([buf[pos + 4], buf[pos + 5], buf[pos + 6], buf[pos + 7]])
            as usize;
        let body = pos + 8;

        if id == b"fmt " {
            if size < 16 {
                return HeaderScan::Invalid("fmt chunk shorter than 16 bytes".into());
            }
            let Some(end) = body.checked_add(size).filter(|end| *end <= buf.len()) else {
                return HeaderScan::NeedMore;
            };
            match parse_fmt(&buf[body..end]) {
                Ok(parsed) => format = Some(parsed),
                Err(reason) => return HeaderScan::Invalid(reason),
            }
        } else if id == b"data" {
            let Some(parsed) = format else {
                return HeaderScan::Invalid("data chunk precedes fmt chunk".into());
            };
            let data_len = if size == 0 || size == u32::MAX as usize {
                u64::MAX
            } else {
                size as u64
            };
            return HeaderScan::Found {
                format: parsed,
                data_offset: body,
                data_len,
            };
        }

        // Chunk bodies are word-aligned.
        let advance = size + (size & 1);
        match body.checked_add(advance) {
            Some(next) if next <= buf.len() => pos = next,
            Some(_) => return HeaderScan::NeedMore,
            None => return HeaderScan::Invalid("chunk size overflows".into()),
        }
    }
}

fn parse_fmt(body: &[u8]) -> Result<PcmFormat, String> {
    let tag = u16::from_le_bytes([body[0], body[1]]);
    let channels = u16::from_le_bytes([body[2], body[3]]);
    let sample_rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
    let bits = u16::from_le_bytes([body[14], body[15]]);

    // WAVE_FORMAT_EXTENSIBLE carries the real tag in the first two bytes of the
    // sub-format GUID (24 bytes into the fmt body).
    let effective_tag = if tag == 0xFFFE {
        if body.len() < 26 {
            return Err("extensible fmt chunk shorter than 26 bytes".into());
        }
        u16::from_le_bytes([body[24], body[25]])
    } else {
        tag
    };

    let sample_format = match (effective_tag, bits) {
        (1, 16) => SampleFormat::PcmI16,
        (1, 24) => SampleFormat::PcmI24,
        (1, 32) => SampleFormat::PcmI32,
        (3, 32) => SampleFormat::F32,
        (1 | 3, other) => {
            return Err(format!("unsupported sample width: {other} bits"));
        }
        (other, _) => {
            return Err(format!("unsupported WAVE format tag: {other}"));
        }
    };

    if channels == 0 {
        return Err("fmt chunk declares zero channels".into());
    }
    if sample_rate == 0 {
        return Err("fmt chunk declares zero sample rate".into());
    }

    Ok(PcmFormat {
        channels,
        sample_rate,
        sample_format,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(channels: u16, rate: u32, bits: u16, frames: &[i16]) -> Vec<u8> {
        let mut data = Vec::new();
        for f in frames {
            for _ in 0..channels {
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
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * channels as u32 * (bits as u32 / 8)).to_le_bytes());
        out.extend_from_slice(&(channels * (bits / 8)).to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn finds_header_and_reports_streaming_data_len() {
        let mut bytes = wav(16, 48_000, 16, &[0, 0]);
        // 2 frames x 16 channels x 2 bytes = 64 bytes of PCM; the data-chunk
        // size field sits immediately before it. A streaming producer writes
        // 0xFFFFFFFF there to mean "until the input ends".
        let size_off = bytes.len() - 64 - 4;
        bytes[size_off..size_off + 4].copy_from_slice(&u32::MAX.to_le_bytes());
        match scan_wav_header(&bytes) {
            HeaderScan::Found {
                format, data_len, ..
            } => {
                assert_eq!(format.channels, 16);
                assert_eq!(format.sample_rate, 48_000);
                assert_eq!(format.sample_format, SampleFormat::PcmI16);
                assert_eq!(data_len, u64::MAX);
            }
            HeaderScan::NeedMore => panic!("unexpected NeedMore"),
            HeaderScan::Invalid(why) => panic!("unexpected Invalid: {why}"),
        }
    }

    #[test]
    fn needs_more_until_header_complete() {
        let bytes = wav(2, 44_100, 16, &[1, 2, 3]);
        for cut in 0..40 {
            assert!(
                matches!(scan_wav_header(&bytes[..cut]), HeaderScan::NeedMore),
                "cut={cut} should be NeedMore"
            );
        }
    }

    #[test]
    fn rejects_non_wave() {
        let bytes = b"RIFF\x10\x00\x00\x00AVI LIST".to_vec();
        assert!(matches!(scan_wav_header(&bytes), HeaderScan::Invalid(_)));
    }

    #[test]
    fn decodes_each_encoding_to_24_bit_scale() {
        assert_eq!(SampleFormat::PcmI16.decode(&100i16.to_le_bytes()), 100 << 8);
        assert_eq!(SampleFormat::PcmI32.decode(&(1i32 << 30).to_le_bytes()), 1 << 22);
        assert_eq!(SampleFormat::F32.decode(&0.5f32.to_le_bytes()), 4_194_303);
        // 24-bit -1 must sign-extend, not read as 0x00FF_FFFF.
        assert_eq!(SampleFormat::PcmI24.decode(&[0xFF, 0xFF, 0xFF]), -1);
    }
}
