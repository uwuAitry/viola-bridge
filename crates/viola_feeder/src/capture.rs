// SPDX-License-Identifier: GPL-3.0-or-later
//! WASAPI capture for the feeder.
//!
//! Two shapes are used, and they are the same call with a different direction:
//!
//! * `Direction::Capture` — record from a real capture endpoint (a microphone,
//!   or a virtual cable's output, e.g. `Voicemeeter Out B1`).
//! * `Direction::Render` — **loopback**: capture whatever is being played to a
//!   playback endpoint (the default output, or a virtual input such as
//!   `Voicemeeter Input`).
//!
//! `autoconvert: true` lets the audio engine resample/reformat into the float
//! layout the bridge wants, so the feeder can ask for 48 kHz f32 regardless of
//! what the endpoint natively runs.

use std::collections::VecDeque;
use wasapi::{
    AudioCaptureClient, AudioClient, Device, DeviceEnumerator, Direction, Handle, SampleType,
    StreamMode, WaveFormat,
};

fn show<E: std::fmt::Display>(err: E) -> String {
    err.to_string()
}

/// Per-poll wait for the capture event, in milliseconds.
const EVENT_TIMEOUT_MS: u32 = 2_000;

/// How many consecutive fruitless polls (≈2 s each) before padding with
/// silence. Measured: a WASAPI loopback stream simply stops delivering while
/// the endpoint is silent, so this is normal, not an error.
const IDLE_POLLS_BEFORE_SILENCE: u32 = 3;

/// `(index, friendly name)` for every endpoint in `direction`.
pub(crate) fn enumerate(direction: &Direction) -> Result<Vec<(u32, String)>, String> {
    let enumerator = DeviceEnumerator::new().map_err(show)?;
    let collection = enumerator.get_device_collection(direction).map_err(show)?;
    let count = collection.get_nbr_devices().map_err(show)?;
    let mut out = Vec::new();
    for index in 0..count {
        let Ok(device) = collection.get_device_at_index(index) else {
            continue;
        };
        let Ok(name) = device.get_friendlyname() else {
            continue;
        };
        out.push((index, name));
    }
    Ok(out)
}

fn find_named(direction: &Direction, needle: &str) -> Result<Device, String> {
    let enumerator = DeviceEnumerator::new().map_err(show)?;
    let collection = enumerator.get_device_collection(direction).map_err(show)?;
    let count = collection.get_nbr_devices().map_err(show)?;
    let needle = needle.to_lowercase();
    for index in 0..count {
        let Ok(device) = collection.get_device_at_index(index) else {
            continue;
        };
        let Ok(name) = device.get_friendlyname() else {
            continue;
        };
        if name.to_lowercase().contains(&needle) {
            return Ok(device);
        }
    }
    Err(format!("no {direction:?} endpoint whose name contains {needle:?}"))
}

/// Choose an endpoint and the direction to open it in.
///
/// * `--device NAME` alone selects a **capture** endpoint by substring.
/// * `--device NAME --loopback` selects a **render** endpoint by substring.
/// * neither selects loopback of the **default render** device, which is the
///   useful default for a smoke test: play anything and it flows through.
pub(crate) fn pick(device: Option<&str>, loopback: bool) -> Result<(Device, Direction), String> {
    match device {
        Some(name) if loopback => Ok((find_named(&Direction::Render, name)?, Direction::Render)),
        Some(name) => Ok((find_named(&Direction::Capture, name)?, Direction::Capture)),
        None => {
            let enumerator = DeviceEnumerator::new().map_err(show)?;
            let device = enumerator.get_default_device(&Direction::Render).map_err(show)?;
            Ok((device, Direction::Render))
        }
    }
}

pub(crate) struct Capture {
    client: AudioClient,
    capture: AudioCaptureClient,
    event: Handle,
    queue: VecDeque<u8>,
    block_align: usize,
}

impl Capture {
    /// Open `device` and start capturing.
    ///
    /// The client is **always** initialised in the capture direction, even when
    /// `device` came from the render collection: that is WASAPI's loopback rule.
    /// Initialising a loopback stream with `Direction::Render` fails with
    /// `AUDCLNT_E_WRONG_ENDPOINT_TYPE` (0x88890003) — measured, hence this note.
    pub(crate) fn open(
        device: &Device,
        sample_rate: usize,
        channels: usize,
    ) -> Result<Self, String> {
        let mut client = device.get_iaudioclient().map_err(show)?;
        let format = WaveFormat::new(32, 32, &SampleType::Float, sample_rate, channels, None);
        let block_align = format.get_blockalign() as usize;
        let (_default_hns, min_hns) = client.get_device_period().map_err(show)?;
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: min_hns,
        };
        client
            .initialize_client(&format, &Direction::Capture, &mode)
            .map_err(show)?;
        let event = client.set_get_eventhandle().map_err(show)?;
        let capture = client.get_audiocaptureclient().map_err(show)?;
        client.start_stream().map_err(show)?;
        Ok(Self {
            client,
            capture,
            event,
            queue: VecDeque::with_capacity(1 << 20),
            block_align,
        })
    }

    /// Block until `frames` interleaved sample-frames are buffered, then return
    /// exactly that many bytes.
    ///
    /// A live feeder must not stall when the endpoint goes quiet: WASAPI
    /// loopback delivers **no packets at all** while nothing is playing (and
    /// `read_from_device_to_deque` still returns `Ok` with nothing added), so
    /// after [`IDLE_POLLS_BEFORE_SILENCE`] fruitless polls this returns a chunk
    /// of silence and flags it, keeping the renderer fed with a continuous
    /// stream instead of spinning or dying.
    pub(crate) fn read_frames(&mut self, frames: usize) -> Result<Chunk, String> {
        let want = frames * self.block_align;
        let mut idle = 0u32;
        while self.queue.len() < want {
            let before = self.queue.len();
            let read_ok = self.capture.read_from_device_to_deque(&mut self.queue).is_ok();
            if read_ok && self.queue.len() > before {
                idle = 0;
            } else {
                idle += 1;
                if idle >= IDLE_POLLS_BEFORE_SILENCE {
                    return Ok(Chunk {
                        bytes: vec![0u8; want],
                        synthetic: true,
                    });
                }
            }
            let _ = self.event.wait_for_event(EVENT_TIMEOUT_MS);
        }
        let mut out = Vec::with_capacity(want);
        for _ in 0..want {
            out.push(self.queue.pop_front().expect("buffered above"));
        }
        Ok(Chunk {
            bytes: out,
            synthetic: false,
        })
    }
}

/// One chunk handed to the pipe.
pub(crate) struct Chunk {
    pub(crate) bytes: Vec<u8>,
    /// `true` when the endpoint was idle and this is padding.
    pub(crate) synthetic: bool,
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.client.stop_stream();
    }
}
