// SPDX-License-Identifier: GPL-3.0-or-later
//! The hand-over from the host's buffer callback to the pipe thread.
//!
//! Two threads touch this, and only two:
//!
//! * the ticker thread (`driver.rs`), inside the host's own buffer callback,
//!   copies one half of the double buffer into the ring, and
//! * the pipe thread (`pipe.rs`), which drains the ring into the named pipe.
//!
//! So this is a single-producer/single-consumer ring and nothing else: no
//! lock, no allocation on either path, no blocking. The callback runs inside
//! the DAW's process, so anything that could block or panic there is a crash
//! of theirs.
//!
//! # Why `UnsafeCell` and a power of two
//!
//! The slots must be mutated through a shared `&Ring` — the callback only
//! holds `&Pipeline` — so they cannot be a plain `Box<[f32]>`. They are a
//! `Box<[UnsafeCell<f32>]>` instead, and the two indices below are the only
//! thing that decides which slot a thread may touch. Capacity is a power of
//! two so the index wrap is a bit-mask (`index & mask`) rather than a
//! division, which is arithmetic an audio callback should not be doing.
//!
//! # Why one slot is always left spare
//!
//! With free-running indices, `len()` is the difference `head - tail`, and a
//! ring allowed to reach full capacity could not tell "full" from "empty" on
//! that difference alone. Rather than keep a second count in step with the
//! first, the writer stops at `capacity - 1`: one slot stays permanently
//! idle, which costs one sample of latency and makes `len() == 0`
//! unambiguously empty and `free() == 0` unambiguous. `capacity()` still
//! reports the allocation size, so the usable count is one less than it.
//!
//! # Why `Release`/`Acquire`
//!
//! The slots hold the data; the index is the flag that says the data is
//! there. The producer writes the block and only then stores `head` with
//! `Release`; the consumer loads `head` with `Acquire` before reading a slot,
//! so the block writes are visible to it. The consumer stores `tail` with
//! `Release` and the producer reads it with `Acquire` for the same reason in
//! the other direction — that is what stops the writer from overwriting a
//! slot the reader has not copied out yet.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::CHANNEL_COUNT;

/// A single-producer/single-consumer ring of interleaved `f32` samples.
///
/// See the module documentation for the wrap rule, the spare slot and the
/// memory ordering. `Ring` is shared by both threads through an `Arc`; every
/// method therefore takes `&self`.
pub(crate) struct Ring {
    /// `capacity` slots, each written by the producer and read by the
    /// consumer under the index protocol above. Allocated once, never resized.
    slots: Box<[UnsafeCell<f32>]>,
    /// `capacity - 1`: the bit-mask that wraps a free-running index.
    mask: usize,
    /// The allocation size, as asked for. The usable count is one less.
    capacity: usize,
    /// Next slot the producer will write; only the producer stores it.
    head: AtomicUsize,
    /// Next slot the consumer will read; only the consumer stores it.
    tail: AtomicUsize,
}

// Safety: `UnsafeCell` is not `Sync` on its own, but the ring only ever has
// one producer and one consumer, and they are separated by the `head`/`tail`
// `Release`/`Acquire` pair. A slot is written only at index `head` (producer)
// and read only below `head` (consumer), so the two never touch the same slot
// at the same time. That is exactly the guarantee `Sync` asks for.
unsafe impl Sync for Ring {}

impl Ring {
    /// `capacity` must be a non-zero power of two. Panics otherwise (this is
    /// only ever called from `start`, on the host's thread, with our own value).
    pub(crate) fn with_capacity_samples(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "ring capacity {capacity} is not a non-zero power of two"
        );
        let slots: Box<[UnsafeCell<f32>]> =
            (0..capacity).map(|_| UnsafeCell::new(0.0)).collect();
        Self {
            slots,
            mask: capacity - 1,
            capacity,
            head: AtomicUsize::new(0),
            tail: AtomicUsize::new(0),
        }
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    /// Samples readable right now.
    pub(crate) fn len(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let tail = self.tail.load(Ordering::Acquire);
        head.wrapping_sub(tail)
    }

    /// Samples writable right now. One slot short of `capacity`, see the
    /// module documentation.
    pub(crate) fn free(&self) -> usize {
        self.capacity - 1 - self.len()
    }

    /// All-or-nothing: writes every sample of `block` or writes none and
    /// returns false. Single producer only.
    pub(crate) fn write_block(&self, block: &[f32]) -> bool {
        if block.len() > self.free() {
            return false;
        }
        // Only this thread ever stores `head`, so a relaxed load of it is
        // enough; the `Release` store below is what publishes the samples.
        let mut head = self.head.load(Ordering::Relaxed);
        for &sample in block {
            // Safety: this thread is the ring's only producer. `free()` proved
            // `block` fits below the consumer's `tail`, so `head` is a slot the
            // consumer may not read, and the mask keeps it inside the
            // allocation.
            unsafe { *self.slots[head & self.mask].get() = sample };
            head = head.wrapping_add(1);
        }
        // Publish: every slot above is written before any consumer can see it.
        self.head.store(head, Ordering::Release);
        true
    }

    /// Copies at most `out.len()` samples; returns how many. Single consumer
    /// only.
    pub(crate) fn read(&self, out: &mut [f32]) -> usize {
        // `len()` is where the `Acquire` on `head` happens, so everything the
        // producer wrote below it is visible from here on.
        let count = out.len().min(self.len());
        // Only this thread ever stores `tail`.
        let mut tail = self.tail.load(Ordering::Relaxed);
        for slot in out.iter_mut().take(count) {
            // Safety: this thread is the ring's only consumer, and `count` was
            // clamped to `len()`, so every index here is one the producer has
            // already published and released.
            *slot = unsafe { *self.slots[tail & self.mask].get() };
            tail = tail.wrapping_add(1);
        }
        // Publish the consumption so the producer may write those slots again.
        self.tail.store(tail, Ordering::Release);
        count
    }

    /// Drops everything unread and returns the count. Consumer only, and only
    /// when the consumer is not going to write that data anywhere.
    pub(crate) fn discard_all(&self) -> usize {
        let head = self.head.load(Ordering::Acquire);
        let dropped = head.wrapping_sub(self.tail.load(Ordering::Relaxed));
        self.tail.store(head, Ordering::Release);
        dropped
    }
}

/// One buffer half lifted out of the host's memory, interleaved.
///
/// The host hands the driver one pointer per activated channel (`common/asio.h`,
/// `ASIOBufferInfo::buffers`); this holds the first half of each and copies the
/// frames into an interleaved scratch buffer the ring can take whole.
pub(crate) struct Tap {
    /// Channel `ch`'s `buffers[0]`, or null when the host did not activate
    /// that channel. The host guarantees these stay valid until it disposes
    /// its buffers (`common/asio.h`: they come from `ASIOBufferCreate` and are
    /// released by `ASIOBufferDispose`).
    outputs: [*const f32; CHANNEL_COUNT as usize],
    /// The buffer size in frames, as `common/asio.h` reports it to
    /// `createBuffers`.
    frames: usize,
    /// `frames * CHANNEL_COUNT` samples, interleaved as
    /// `scratch[frame * CHANNEL_COUNT + channel]`. Sized in `new`, never
    /// resized, so `fill` cannot allocate. Interior-mutable because `Tick` is
    /// shared as an `Arc` and there is no `&mut Tap` to be had.
    scratch: UnsafeCell<Box<[f32]>>,
}

// The output pointers belong to the host (`common/asio.h`: it fills them in
// during ASIOBufferCreate and keeps them valid until ASIOBufferDispose, which
// the driver calls only after the ticker has stopped). `Tick` travels between
// threads as an `Arc`, so this type needs both bounds: `Send` because the raw
// pointers are not, and `Sync` because of the `UnsafeCell` scratch.
unsafe impl Send for Tap {}

// Sound because exactly one thread — the ticker — ever calls `fill`, it is the
// only thread that reads the returned slice, and the slice is consumed
// (`write_block`) before the next `fill`. Nothing else in the process reads or
// writes the scratch.
unsafe impl Sync for Tap {}

impl Tap {
    /// `outputs[ch]` is channel `ch`'s `buffers[0]`, or null if the host did not
    /// activate that channel. `frames` is the buffer size.
    pub(crate) fn new(outputs: [*const f32; CHANNEL_COUNT as usize], frames: usize) -> Self {
        let samples = frames * CHANNEL_COUNT as usize;
        Self {
            outputs,
            frames,
            scratch: UnsafeCell::new(vec![0.0_f32; samples].into_boxed_slice()),
        }
    }

    /// Copies frame `half + j` of every channel into the interleaved scratch and
    /// returns it: `out[j * CHANNEL_COUNT + ch]`. `half` is 0 or `frames`.
    /// A null channel contributes silence. The tap's own frame count is fixed
    /// in `new`; `half` outside those two values is a caller bug.
    pub(crate) fn fill(&self, half: usize) -> &[f32] {
        debug_assert!(
            half == 0 || half == self.frames,
            "a buffer half must be 0 or the buffer size ({}), got {half}",
            self.frames
        );
        // Safety: this is the only thread that ever calls `fill` (see the
        // `Sync` assertion above), so no other borrow of the scratch can exist
        // while this one does.
        let scratch = unsafe { &mut *self.scratch.get() };
        let frames = self.frames;
        let channels = CHANNEL_COUNT as usize;
        for channel in 0..channels {
            let source = self.outputs[channel];
            for frame in 0..frames {
                let sample = if source.is_null() {
                    // The host never activated this channel.
                    0.0
                } else {
                    // Safety: `source` is a non-null channel half the host said
                    // holds `frames` frames, and `half + frame` is inside that
                    // half because `half` is 0 or `frames`.
                    unsafe { *source.add(half + frame) }
                };
                scratch[frame * channels + channel] = sample;
            }
        }
        &scratch[..]
    }
}
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ptr;

    /// Write past the mask, read it back, and check the values keep their order
    /// across the wrap.
    #[test]
    fn writes_and_reads_wrap_around_the_mask() {
        let ring = Ring::with_capacity_samples(8);
        assert_eq!(ring.capacity(), 8);
        // One slot is permanently spare.
        assert_eq!(ring.free(), 7);

        assert!(ring.write_block(&[1.0, 2.0, 3.0, 4.0, 5.0]));
        assert_eq!(ring.len(), 5);

        // Free three slots, so head is ahead of tail by two and the next block
        // must wrap past index 7 back to 0.
        let mut out = [0.0_f32; 3];
        assert_eq!(ring.read(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
        assert_eq!(ring.len(), 2);

        assert!(ring.write_block(&[6.0, 7.0, 8.0, 9.0]));
        assert_eq!(ring.len(), 6);

        let mut rest = [0.0_f32; 6];
        assert_eq!(ring.read(&mut rest), 6);
        assert_eq!(rest, [4.0, 5.0, 6.0, 7.0, 8.0, 9.0]);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.free(), 7);
    }

    /// A block that does not fit is refused whole, and the ring keeps every
    /// sample it already held.
    #[test]
    fn a_full_ring_refuses_a_whole_block() {
        let ring = Ring::with_capacity_samples(4);
        assert!(ring.write_block(&[1.0, 2.0, 3.0]));
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.free(), 0);

        assert!(!ring.write_block(&[4.0]));
        // Even partially: a block is taken whole or not at all.
        assert!(!ring.write_block(&[4.0, 5.0]));
        assert_eq!(ring.len(), 3);

        let mut out = [0.0_f32; 3];
        assert_eq!(ring.read(&mut out), 3);
        assert_eq!(out, [1.0, 2.0, 3.0]);
    }

    /// Dropping the backlog reports what went, empties the ring, and gives the
    /// space back.
    #[test]
    fn discard_all_empties_the_ring() {
        let ring = Ring::with_capacity_samples(4);
        assert!(ring.write_block(&[1.0, 2.0, 3.0]));

        assert_eq!(ring.discard_all(), 3);
        assert_eq!(ring.len(), 0);
        assert_eq!(ring.free(), 3);
        assert_eq!(ring.discard_all(), 0);

        assert!(ring.write_block(&[4.0, 5.0, 6.0]));
        let mut out = [0.0_f32; 3];
        assert_eq!(ring.read(&mut out), 3);
        assert_eq!(out, [4.0, 5.0, 6.0]);
    }

    /// The tap interleaves `out[frame * 16 + channel]` and gives a
    /// non-activated channel silence rather than reading a null pointer.
    #[test]
    fn tap_interleaves_channels_and_zero_fills_the_missing_ones() {
        const FRAMES: usize = 4;
        const CHANNELS: usize = CHANNEL_COUNT as usize;

        // The per-channel halves the host would own: two `FRAMES`-long halves
        // back to back, which is what `buffers[1] = buffers[0] + buffer_size`
        // means (`common/asio.h`). They must outlive the tap, exactly as the
        // host's buffers outlive `disposeBuffers`.
        let halves: Vec<[f32; FRAMES * 2]> = (0..CHANNELS)
            .map(|channel| {
                let mut half = [0.0_f32; FRAMES * 2];
                for (frame, slot) in half.iter_mut().enumerate() {
                    *slot = (channel * 100 + frame) as f32;
                }
                half
            })
            .collect();

        let mut outputs: [*const f32; CHANNELS] = [ptr::null(); CHANNELS];
        for (channel, half) in halves.iter().enumerate() {
            outputs[channel] = half.as_ptr();
        }
        // Two channels the host did not activate.
        outputs[5] = ptr::null();
        outputs[11] = ptr::null();

        let tap = Tap::new(outputs, FRAMES);
        let out = tap.fill(0);
        assert_eq!(out.len(), FRAMES * CHANNELS);
        for channel in 0..CHANNELS {
            for frame in 0..FRAMES {
                let expected = if channel == 5 || channel == 11 {
                    0.0
                } else {
                    (channel * 100 + frame) as f32
                };
                assert_eq!(
                    out[frame * CHANNELS + channel],
                    expected,
                    "channel {channel} frame {frame}"
                );
            }
        }

        // The second half of the host's double buffer is a `frames` offset into
        // the same channel pointers, and reads the same way.
        let second = tap.fill(FRAMES);
        assert_eq!(second.len(), FRAMES * CHANNELS);
        assert_eq!(second[1 * CHANNELS + 3], (3 * 100 + FRAMES + 1) as f32);
        assert_eq!(second[0 * CHANNELS + 5], 0.0);
    }
}
