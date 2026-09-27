// SPDX-License-Identifier: GPL-3.0-or-later
//! viola-bridge — a channel-bed decoder bridge for the Omniphony renderer.
//!
//! `orender` loads a decoder bridge as a runtime plugin; without one it refuses
//! to start. This crate is that plugin for plain PCM: it presents a streamed
//! multichannel WAV or a headerless interleaved PCM stream as a channel bed,
//! which the renderer then spatialises (VBAP / binaural) according to each
//! channel's `RChannelLabel`.
//!
//! Its reason to exist over the upstream reference bridge is the **9.1.6
//! (16-channel) label map** and the ability to take its layout from the
//! environment for a headerless live stream — the shape a pipe-fed feeder
//! produces.
//!
//! Environment variables (read when a bridge instance is created):
//!
//! | variable | default | meaning |
//! |---|---|---|
//! | `VIOLA_BRIDGE_MODE` | `auto` | `auto` / `wav` / `raw` |
//! | `VIOLA_BRIDGE_CHANNELS` | `16` | channel count for headerless PCM |
//! | `VIOLA_BRIDGE_RATE` | `48000` | sample rate for headerless PCM |
//! | `VIOLA_BRIDGE_FORMAT` | `f32` | `f32` / `s16` / `s24` / `s32` |
//!
//! The plugin is licensed GPL-3.0-or-later because it links `bridge_api`, which
//! is GPL-3.0-or-later. It must not be linked into a proprietary program.

#![allow(non_local_definitions)]

mod bridge;
mod pcm;

use abi_stable::{
    export_root_module, prefix_type::PrefixTypeTrait, sabi_trait::prelude::TD_Opaque,
};
use bridge::ViolaBridge;
use bridge_api::{BridgeLib, BridgeLibRef, FormatBridgeBox, FormatBridge_TO};

// `FormatBridge` is reached through the proc-macro generated trait object impl.
#[allow(unused_imports)]
use bridge_api::FormatBridge as _FormatBridgeTrait;

/// Plugin entry point: export the root module the host looks for.
///
/// `abi_stable` requires the module to be named `format_bridge` — that is what
/// `orender` resolves after loading the shared library.
#[export_root_module]
fn get_library() -> BridgeLibRef {
    BridgeLib {
        new_bridge: create_bridge,
        // Diagnostics go to stderr; this bridge installs no host log sink.
        set_host_log_sink: set_host_log_sink,
    }
    .leak_into_prefix()
}

extern "C" fn create_bridge(strict: bool) -> FormatBridgeBox {
    FormatBridge_TO::from_value(ViolaBridge::new(strict), TD_Opaque)
}

extern "C" fn set_host_log_sink(_sink: usize) {}
