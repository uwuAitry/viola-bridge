// SPDX-License-Identifier: GPL-3.0-or-later
//! The pipe side of the M5.3 path: draining the ring into `\\.\pipe\orender.input`.
//!
//! We are the **client** side of the pipe. Measured behaviour of `orender`
//! (`README.md`, and the same conclusion `viola_feeder/src/pipe.rs` reached for
//! its own connection): when the pipe does not exist yet the engine creates it
//! and waits for a client, so our side only has to open the path, which
//! `std::fs` can do on Windows. No Win32 named-pipe FFI, and no `windows`
//! crate.
//!
//! The engine tears the pipe down and re-creates it between streams
//! (`viola_feeder/src/pipe.rs`), so a dropped connection is normal here too: we
//! reconnect and re-send the WAV header rather than treating it as fatal.
//!
//! Reconnecting always costs us the audio queued while we were away — and the
//! backlog from before the engine vanished is not audio anybody wants — so the
//! ring is drained on the way in and again straight after the open, before the
//! header goes out. That is what keeps the latency from creeping up when a
//! stalled start or a reconnect is slow.
//!
//! Nothing here may block indefinitely: the pipe thread is signalled through
//! `stop`, and every wait is a short slice that re-checks it.

use core::mem::size_of;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crate::CHANNEL_COUNT;
use crate::ring::Ring;

/// The float samples each drain pass asks the ring for. The ring holds more
/// than this at the default buffer size, but a smaller local buffer only means
/// the drain takes a few more passes. Nothing is drained until it has been read
/// out of the ring, so this size costs latency, not samples.
const DRAIN_SAMPLES: usize = 8192;

/// How long a failed connect or a failed write waits before trying the path
/// again. Short, because the engine may be re-creating the pipe right now.
const RECONNECT_DELAY: Duration = Duration::from_millis(200);

/// How long an empty ring waits before being asked again. The ticker produces
/// one block every few milliseconds, so this is only a busy-loop brake.
const IDLE_DELAY: Duration = Duration::from_millis(2);

/// The longest a single sleep is allowed to run before `stop` is looked at
/// again, so shutdown never waits for a whole `RECONNECT_DELAY`.
const SLEEP_SLICE: Duration = Duration::from_millis(20);

/// The 44-byte RIFF/WAVE header viola_feeder already sends
/// (`viola_feeder/src/wav.rs`): WAVE_FORMAT_IEEE_FLOAT (3), both sizes
/// `u32::MAX`, i.e. "until the input ends".
///
/// `channels`/`bits` are parameters rather than `CHANNEL_COUNT`/32 constants so
/// the one caller has to state the format it is actually sending, and so the
/// test can pin the numbers down. `bits` must be 32 for the payload below to be
/// right.
pub(crate) fn streaming_header(sample_rate: u32, channels: u16, bits: u16) -> [u8; 44] {
    // Both are derived the way `viola_feeder/src/wav.rs` derives them: an
    // integral number of bytes per frame, and that times the rate.
    let block_align = channels * bits / 8;
    let byte_rate = sample_rate * u32::from(block_align);

    let mut out = [0_u8; 44];
    out[0..4].copy_from_slice(b"RIFF");
    out[4..8].copy_from_slice(&u32::MAX.to_le_bytes()); // size unknown while streaming
    out[8..12].copy_from_slice(b"WAVE");
    out[12..16].copy_from_slice(b"fmt ");
    out[16..20].copy_from_slice(&16_u32.to_le_bytes()); // PCM-style fmt chunk
    out[20..22].copy_from_slice(&3_u16.to_le_bytes()); // WAVE_FORMAT_IEEE_FLOAT
    out[22..24].copy_from_slice(&channels.to_le_bytes());
    out[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    out[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    out[32..34].copy_from_slice(&block_align.to_le_bytes());
    out[34..36].copy_from_slice(&bits.to_le_bytes());
    out[36..40].copy_from_slice(b"data");
    out[40..44].copy_from_slice(&u32::MAX.to_le_bytes()); // until the input ends
    out
}

/// Everything the pipe thread shares with the timer thread.
pub(crate) struct Pipeline {
    pub(crate) ring: Ring,
    /// Blocks the timer thread could not hand over because the ring was full.
    pub(crate) dropped_blocks: AtomicU64,
    /// Interleaved samples actually written into the pipe.
    pub(crate) samples_written: AtomicU64,
    /// Whether the pipe is currently connected - for the log line only.
    pub(crate) connected: AtomicBool,
}

impl Pipeline {
    /// `capacity_samples` is the ring's capacity in interleaved samples, and
    /// must be a power of two (the ring asserts it). The driver's choice of
    /// number is its own business; the shortcut `new` exists so it does not
    /// have to name `Ring`'s constructor itself.
    pub(crate) fn new(capacity_samples: usize) -> Self {
        Self {
            ring: Ring::with_capacity_samples(capacity_samples),
            dropped_blocks: AtomicU64::new(0),
            samples_written: AtomicU64::new(0),
            connected: AtomicBool::new(false),
        }
    }
}

/// Opens `path` as a client, drains the ring into it, reconnects when orender
/// tears the pipe down, and returns once `stop` is set.
pub(crate) fn pipe_loop(
    pipeline: Arc<Pipeline>,
    stop: Arc<AtomicU32>,
    path: PathBuf,
    sample_rate: u32,
) {
    // The header is written from the rate captured at start, so a later
    // `setSampleRate` cannot make the stream's declared rate disagree with the
    // one the driver is running at.
    let header = streaming_header(sample_rate, CHANNEL_COUNT as u16, 32);
    // Allocated once, outside both loops: the drain path must not allocate.
    let mut samples = vec![0.0_f32; DRAIN_SAMPLES];

    while stop.load(Ordering::Acquire) == 0 {
        pipeline.connected.store(false, Ordering::Release);
        // Whatever the ticker queued while we had no pipe is stale by now.
        pipeline.ring.discard_all();

        // The client side: `viola_feeder/src/pipe.rs` opens the same path the
        // same way, and the engine creates the pipe and waits for us
        // (`README.md`).
        let opened = File::options().write(true).open(&path);
        let Ok(mut file) = opened else {
            // The engine may not have created the pipe yet, which is an
            // ordinary "file not found" here. Try again shortly.
            sleep_in_slices(&stop, RECONNECT_DELAY);
            continue;
        };

        // The open can itself take a while, so drop again before the header
        // rather than trusting the discard above.
        pipeline.ring.discard_all();
        if file.write_all(&header).is_err() || file.flush().is_err() {
            // Drop the handle: the next attempt reconnects from scratch.
            continue;
        }
        pipeline.connected.store(true, Ordering::Release);

        while stop.load(Ordering::Acquire) == 0 {
            let read = pipeline.ring.read(&mut samples);
            if read == 0 {
                sleep_in_slices(&stop, IDLE_DELAY);
                continue;
            }

            // The wire format is interleaved `f32` little-endian, which is what
            // the header above declares (`WAVE_FORMAT_IEEE_FLOAT`) and what
            // asio.h calls `ASIOSTFloat32LSB`. On the x86_64 target this
            // reinterpretation is just a view of the same bytes.
            //
            // Safety: `read` is at most `samples.len()`, so the byte range is
            // inside the allocation, and a `u8` slice has no alignment demand.
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    samples.as_ptr().cast::<u8>(),
                    read * size_of::<f32>(),
                )
            };
            if file.write_all(bytes).is_err() || file.flush().is_err() {
                // A write failure means the engine went away: back to the
                // connect path, which discards the ring on the way through.
                break;
            }
            pipeline
                .samples_written
                .fetch_add(read as u64, Ordering::Release);
        }

        // Either `stop` was set or a write failed; both leave the handle to be
        // dropped here, and the outer loop decides which.
        pipeline.connected.store(false, Ordering::Release);
    }
}

/// Sleep for `total` in `SLEEP_SLICE` steps, giving up as soon as `stop` is set.
///
/// This is the whole of `pipe_loop`'s shutdown latency: no wait ever runs the
/// full reconnect delay without looking at `stop` first.
fn sleep_in_slices(stop: &AtomicU32, total: Duration) {
    let mut slept = Duration::ZERO;
    while slept < total {
        if stop.load(Ordering::Acquire) != 0 {
            return;
        }
        let slice = SLEEP_SLICE.min(total - slept);
        thread::sleep(slice);
        slept += slice;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_44_bytes_and_names_sixteen_channels() {
        let header = streaming_header(48_000, 16, 32);

        assert_eq!(header.len(), 44);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        assert_eq!(&header[12..16], b"fmt ");
        assert_eq!(&header[36..40], b"data");

        // The two sizes are "unknown while streaming".
        assert_eq!(&header[4..8], &u32::MAX.to_le_bytes());
        assert_eq!(&header[40..44], &u32::MAX.to_le_bytes());

        // WAVE_FORMAT_IEEE_FLOAT (3), per `viola_feeder/src/wav.rs`.
        assert_eq!(u16::from_le_bytes([header[20], header[21]]), 3);
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 16);
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            48_000
        );
        // 48000 * 16 * 32/8 bytes per second.
        assert_eq!(
            u32::from_le_bytes([header[28], header[29], header[30], header[31]]),
            3_072_000
        );
        // 16 channels * 32 bits / 8.
        assert_eq!(u16::from_le_bytes([header[32], header[33]]), 64);
        assert_eq!(u16::from_le_bytes([header[34], header[35]]), 32);
    }
}
