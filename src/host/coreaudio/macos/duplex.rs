//! Duplex callback wrapper machinery for CoreAudio.
//!
//! `coreaudio-rs` does not expose a builder for duplex AudioUnits (a single HALOutput unit with
//! both input and output buses enabled), so the duplex path constructs the callback closure here
//! and registers it via a raw `AURenderCallbackStruct`. This module owns the wrapper type that
//! `StreamInner::duplex_callback_ptr` points to, and the `extern "C-unwind"` entry point that
//! bridges from CoreAudio's render thread back into Rust.
//!
//! `Device::build_duplex_stream_raw` (added in a follow-up commit) constructs the wrapper,
//! `Box::into_raw`s it, and registers it via `kAudioUnitProperty_SetRenderCallback` with
//! [`duplex_input_proc`] as the entry function.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::NonNull;

use objc2_audio_toolbox::AudioUnitRenderActionFlags;
use objc2_core_audio_types::{kAudio_ParamError, AudioBufferList, AudioTimeStamp};

/// Closure signature invoked from [`duplex_input_proc`]. The closure owns the data callback,
/// the input scratch buffer, and the error callback; it returns a CoreAudio status code.
///
/// `Send` is asserted at the wrapper level (`unsafe impl Send for DuplexProcWrapper`), not on
/// the boxed closure — captures like the raw `AudioUnit` handle are not auto-`Send`, and we
/// uphold the soundness invariant out-of-band (`Drop for StreamInner` stops the audio unit
/// before the wrapper is reclaimed, so the closure never runs on more than one thread).
pub(super) type DuplexProcFn = dyn FnMut(
    NonNull<AudioUnitRenderActionFlags>,
    NonNull<AudioTimeStamp>,
    u32, // bus number
    u32, // frame count
    *mut AudioBufferList,
) -> i32;

/// Boxed render callback shared with CoreAudio via `inputProcRefCon`.
///
/// The wrapper is heap-allocated and leaked via `Box::into_raw` once, then reclaimed by
/// `StreamInner::drop` after the audio unit has been stopped. CoreAudio invokes
/// [`duplex_input_proc`] on its render thread for the lifetime of the audio unit; that function
/// reconstructs `&mut DuplexProcWrapper` from the raw pointer and calls the boxed closure.
//
// `dead_code` is allowed because `Device::build_duplex_stream_raw` (the only constructor) and
// `duplex_input_proc` (the only reader) are added in the next commit.
#[allow(dead_code)]
pub(super) struct DuplexProcWrapper {
    pub(super) callback: Box<DuplexProcFn>,
}

// SAFETY: the `callback` field is a `Box<DuplexProcFn>` where `DuplexProcFn: Send`. The pointer
// itself is never accessed concurrently — CoreAudio's render thread is the only reader during
// the audio unit's lifetime, and the build/drop thread only writes/reclaims when the audio unit
// is stopped (see `Drop for StreamInner`).
unsafe impl Send for DuplexProcWrapper {}

/// CoreAudio render callback entry point.
///
/// `extern "C-unwind"` matches `AURenderCallbackStruct::inputProc`. We wrap the closure
/// invocation in `catch_unwind` so a panic in user code returns `kAudio_ParamError` rather than
/// unwinding through CoreAudio's C frames (undefined behavior).
///
/// # Safety
///
/// - `in_ref_con` must point to a `DuplexProcWrapper` created via `Box::into_raw` and not yet
///   reclaimed. `Drop for StreamInner` stops the audio unit before reclaiming, so within the
///   lifetime of the audio unit this is upheld.
/// - CoreAudio invokes this function from a single render thread per audio unit, so the
///   `&mut DuplexProcWrapper` we materialize is the only outstanding reference.
// `dead_code` allow: caller is `Device::build_duplex_stream_raw`, added in the next commit.
#[allow(dead_code)]
pub(super) extern "C-unwind" fn duplex_input_proc(
    in_ref_con: NonNull<c_void>,
    io_action_flags: NonNull<AudioUnitRenderActionFlags>,
    in_time_stamp: NonNull<AudioTimeStamp>,
    in_bus_number: u32,
    in_number_frames: u32,
    io_data: *mut AudioBufferList,
) -> i32 {
    // SAFETY: see function-level safety doc. The wrapper outlives the audio unit; the audio
    // thread is the sole concurrent reader.
    let wrapper = unsafe { in_ref_con.cast::<DuplexProcWrapper>().as_mut() };
    match catch_unwind(AssertUnwindSafe(|| {
        (wrapper.callback)(
            io_action_flags,
            in_time_stamp,
            in_bus_number,
            in_number_frames,
            io_data,
        )
    })) {
        Ok(status) => status,
        Err(_) => kAudio_ParamError,
    }
}
