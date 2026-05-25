//! Types for duplex (simultaneous input/output) audio streams.

use crate::{BufferSize, ChannelCount, InputStreamTimestamp, OutputStreamTimestamp, SampleRate};

/// Information relevant to a single call to the user's duplex stream data callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DuplexCallbackInfo {
    input_timestamp: InputStreamTimestamp,
    output_timestamp: OutputStreamTimestamp,
}

impl DuplexCallbackInfo {
    pub fn new(
        input_timestamp: InputStreamTimestamp,
        output_timestamp: OutputStreamTimestamp,
    ) -> Self {
        Self {
            input_timestamp,
            output_timestamp,
        }
    }

    /// The timestamp for the input portion of the duplex callback.
    pub fn input_timestamp(&self) -> InputStreamTimestamp {
        self.input_timestamp
    }

    /// The timestamp for the output portion of the duplex callback.
    pub fn output_timestamp(&self) -> OutputStreamTimestamp {
        self.output_timestamp
    }
}

/// The set of parameters used to describe how to open a duplex stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DuplexStreamConfig {
    pub input_channels: ChannelCount,
    pub output_channels: ChannelCount,
    pub sample_rate: SampleRate,
    pub buffer_size: BufferSize,
}
