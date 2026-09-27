// SPDX-License-Identifier: GPL-3.0-or-later
//! Builds the RIFF/WAVE header this feeder writes once per engine connection.
//!
//! The `data` chunk size is `0xFFFFFFFF` — "until the input ends" — which is
//! what a live producer must write. `viola_bridge` accepts that value
//! (`scan_wav_header` maps `0` and `0xFFFFFFFF` to `u64::MAX`), so the format
//! travels in-band and no environment variable has to be kept in sync between
//! the feeder and the engine process.

/// 44-byte RIFF/WAVE header for a `bits`-wide float stream, with an unbounded
/// `data` chunk.
pub(crate) fn streaming_header(sample_rate: u32, channels: u16, bits: u16) -> Vec<u8> {
    let block_align = channels * bits / 8;
    let byte_rate = sample_rate * block_align as u32;
    let mut out = Vec::with_capacity(44);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::MAX.to_le_bytes()); // size unknown while streaming
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM-style fmt chunk
    out.extend_from_slice(&3u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&u32::MAX.to_le_bytes()); // until the input ends
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_44_bytes_and_round_trips_through_the_bridge_scanner() {
        let header = streaming_header(48_000, 16, 32);
        assert_eq!(header.len(), 44);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        assert_eq!(&header[36..40], b"data");
        assert_eq!(&header[40..44], &u32::MAX.to_le_bytes());
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 16);
        assert_eq!(u32::from_le_bytes([header[24], header[25], header[26], header[27]]), 48_000);
        assert_eq!(u16::from_le_bytes([header[34], header[35]]), 32);
    }
}
