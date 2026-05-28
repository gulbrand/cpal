//! Types for building synchronized duplex (simultaneous input and output) streams.
//!
//! A duplex stream pairs an input and an output direction on a single device-level callback so
//! that captured and rendered audio share the same hardware clock. This is appropriate for
//! workloads that must align input and output sample-for-sample without separate ring buffers
//! and resampling (e.g. low-latency effects processing).
//!
//! Support is opt-in per [`Host`](crate::Host): see
//! [`DeviceTrait::supports_duplex`](crate::traits::DeviceTrait::supports_duplex) and
//! [`DeviceTrait::build_duplex_stream`](crate::traits::DeviceTrait::build_duplex_stream).

use crate::{BufferSize, ChannelCount, InputStreamTimestamp, OutputStreamTimestamp, SampleRate};

/// Information relevant to a single call to the user's duplex stream data callback.
///
/// Carries timestamps for both the captured input frame and the rendered output frame. The two
/// timestamps share the same `callback` instant because input and output are driven by the same
/// underlying device callback.
#[derive(Copy, Clone, Debug, Eq, Hash, PartialEq)]
pub struct DuplexCallbackInfo {
    pub(crate) input_timestamp: InputStreamTimestamp,
    pub(crate) output_timestamp: OutputStreamTimestamp,
}

impl DuplexCallbackInfo {
    /// Construct a new `DuplexCallbackInfo` from input and output timestamps.
    pub fn new(
        input_timestamp: InputStreamTimestamp,
        output_timestamp: OutputStreamTimestamp,
    ) -> Self {
        Self {
            input_timestamp,
            output_timestamp,
        }
    }

    /// The timestamp associated with the captured input frame.
    pub fn input_timestamp(&self) -> InputStreamTimestamp {
        self.input_timestamp
    }

    /// The timestamp associated with the rendered output frame.
    pub fn output_timestamp(&self) -> OutputStreamTimestamp {
        self.output_timestamp
    }
}

/// The set of parameters used to open a duplex stream.
///
/// Input and output share `sample_rate` and `buffer_size`, but may have different channel counts.
/// The sample format is provided separately, mirroring [`StreamConfig`](crate::StreamConfig).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DuplexStreamConfig {
    /// Number of channels captured from the input direction.
    pub input_channels: ChannelCount,
    /// Number of channels rendered to the output direction.
    pub output_channels: ChannelCount,
    /// Sample rate shared by both directions.
    pub sample_rate: SampleRate,
    /// Buffer size shared by both directions. See [`BufferSize`].
    pub buffer_size: BufferSize,
}
