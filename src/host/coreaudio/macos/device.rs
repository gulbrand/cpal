use std::{
    ffi::c_void,
    fmt,
    mem::{self, size_of, ManuallyDrop},
    ptr::{null, NonNull},
    sync::{
        mpsc::{channel, RecvTimeoutError},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use coreaudio::audio_unit::{
    audio_format::LinearPcmFlags,
    macos_helpers::{
        audio_unit_from_device_id_uninitialized, find_matching_physical_format, get_device_name,
        set_device_physical_stream_format, RateListener,
    },
    render_callback::{self, data},
    AudioUnit, Element, SampleFormat as CoreAudioSampleFormat, Scope, StreamFormat,
};
use objc2_audio_toolbox::{
    kAudioOutputUnitProperty_CurrentDevice, kAudioOutputUnitProperty_EnableIO,
    kAudioUnitProperty_SetRenderCallback, kAudioUnitProperty_StreamFormat, AURenderCallbackStruct,
    AudioUnitRender, AudioUnitRenderActionFlags,
};
use objc2_core_audio::{
    kAudioAggregateDeviceClassID, kAudioDevicePropertyAvailableNominalSampleRates,
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyBufferFrameSizeRange,
    kAudioDevicePropertyDeviceUID, kAudioDevicePropertyLatency,
    kAudioDevicePropertyNominalSampleRate, kAudioDevicePropertySafetyOffset,
    kAudioDevicePropertyStreamConfiguration, kAudioDevicePropertyStreamFormat,
    kAudioObjectPropertyClass, kAudioObjectPropertyElementMain, kAudioObjectPropertyElementMaster,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput,
    kAudioObjectPropertyScopeOutput, AudioClassID, AudioDeviceID, AudioObjectGetPropertyData,
    AudioObjectGetPropertyDataSize, AudioObjectID, AudioObjectPropertyAddress,
    AudioObjectPropertyScope, AudioObjectSetPropertyData,
};
use objc2_core_audio_types::{
    kAudio_ParamError, AudioBuffer, AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp,
    AudioValueRange,
};
use objc2_core_foundation::{CFString, Type};

use super::duplex::{duplex_input_proc, DuplexProcWrapper};
pub use super::enumerate::{SupportedInputConfigs, SupportedOutputConfigs};
use super::{
    asbd_from_config, check_os_status, host_time_to_stream_instant, DefaultOutputMonitor,
    DisconnectManager, DuplexCallbackPtr, Monitor, Stream,
};
use crate::{
    host::{
        coreaudio::macos::{loopback::LoopbackDevice, StreamInner},
        frames_to_duration, try_emit_error, ErrorCallbackArc,
    },
    traits::DeviceTrait,
    BufferSize, ChannelCount, Data, DeviceDescription, DeviceDescriptionBuilder, DeviceId,
    DuplexCallbackInfo, DuplexStreamConfig, Error, ErrorKind, FrameCount, InputCallbackInfo,
    InputStreamTimestamp, InterfaceType, OutputCallbackInfo, OutputStreamTimestamp, ResultExt,
    SampleFormat, SampleRate, StreamConfig, StreamInstant, SupportedBufferSize,
    SupportedStreamConfig, SupportedStreamConfigRange,
};

/// Try to find a matching physical stream format on the device and apply it.
///
/// Setting the physical format ensures the hardware runs at the requested bit depth and sample
/// rate without unnecessary conversions.
fn set_physical_format(
    device_id: AudioDeviceID,
    sample_rate: SampleRate,
    channels: ChannelCount,
    sample_format: SampleFormat,
) -> Result<AudioStreamBasicDescription, coreaudio::Error> {
    let core_format = match sample_format {
        SampleFormat::I8 => CoreAudioSampleFormat::I8,
        SampleFormat::I16 => CoreAudioSampleFormat::I16,
        SampleFormat::I24 => CoreAudioSampleFormat::I24,
        SampleFormat::I32 => CoreAudioSampleFormat::I32,
        SampleFormat::F32 => CoreAudioSampleFormat::F32,
        _ => return Err(coreaudio::Error::UnsupportedStreamFormat),
    };
    let stream_format = StreamFormat {
        sample_rate: sample_rate as f64,
        sample_format: core_format,
        flags: LinearPcmFlags::empty(),
        channels: channels as u32,
    };
    let asbd = find_matching_physical_format(device_id, stream_format)
        .ok_or(coreaudio::Error::UnsupportedStreamFormat)?;
    set_device_physical_stream_format(device_id, asbd).map(|_| asbd)
}

/// Set the device's nominal sample rate via `kAudioDevicePropertyNominalSampleRate`.
///
/// Unlike [`set_physical_format`], this only changes the device clock rate. The AudioUnit bridges
/// any remaining format difference to the virtual stream format seen by the callback.
fn set_sample_rate(
    audio_device_id: AudioObjectID,
    target_sample_rate: SampleRate,
    timeout: Option<Duration>,
) -> Result<(), Error> {
    // Get the current sample rate.
    let mut property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyNominalSampleRate,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };
    let mut sample_rate: f64 = 0.0;
    let mut data_size = mem::size_of::<f64>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            audio_device_id,
            NonNull::from(&property_address),
            0,
            null(),
            NonNull::from(&mut data_size),
            NonNull::from(&mut sample_rate).cast(),
        )
    };
    coreaudio::Error::from_os_status(status)?;

    // If the requested sample rate is different to the device sample rate, update the device.
    if (sample_rate - target_sample_rate as f64).abs() >= 1.0 {
        // Get available sample rate ranges.
        property_address.mSelector = kAudioDevicePropertyAvailableNominalSampleRates;
        let mut data_size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(
                audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
            )
        };
        coreaudio::Error::from_os_status(status)?;
        let n_ranges = data_size as usize / mem::size_of::<AudioValueRange>();
        let mut ranges: Vec<AudioValueRange> = Vec::with_capacity(n_ranges);
        let status = unsafe {
            AudioObjectGetPropertyData(
                audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
                NonNull::new(ranges.as_mut_ptr()).unwrap().cast(),
            )
        };
        coreaudio::Error::from_os_status(status)?;
        unsafe {
            ranges.set_len(n_ranges);
        }

        // Now that we have the available ranges, pick the one matching the desired rate.
        let sample_rate = target_sample_rate;
        if !ranges
            .iter()
            .any(|r| sample_rate as f64 >= r.mMinimum && sample_rate as f64 <= r.mMaximum)
        {
            return Err(Error::with_message(
                ErrorKind::UnsupportedConfig,
                format!("Sample rate {sample_rate} Hz is not supported"),
            ));
        }

        // Register the listener before setting the property so we don't miss the notification.
        let (sender, receiver) = channel::<f64>();
        let mut listener = RateListener::new(audio_device_id, Some(sender));
        listener.register()?;

        // Set the nominal sample rate.
        property_address.mSelector = kAudioDevicePropertyNominalSampleRate;
        let rate = sample_rate as f64;
        let data_size = mem::size_of::<f64>() as u32;
        let status = unsafe {
            AudioObjectSetPropertyData(
                audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                data_size,
                NonNull::from(&rate).cast(),
            )
        };
        coreaudio::Error::from_os_status(status)?;

        // Wait for the reported_rate to change.
        //
        // This should not take longer than a few ms. Use the caller's timeout if provided,
        // otherwise default to 1 second. We loop over potentially several events from the
        // channel to ensure that we catch the expected change in sample rate.
        let mut remaining = timeout.unwrap_or(Duration::from_secs(1));
        let start = Instant::now();
        loop {
            match receiver.recv_timeout(remaining) {
                Ok(reported_rate) => {
                    if (reported_rate - target_sample_rate as f64).abs() < 1.0 {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(Error::with_message(
                        ErrorKind::DeviceNotAvailable,
                        "Sample rate update timed out",
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Error::with_message(
                        ErrorKind::StreamInvalidated,
                        "Sample rate listener disconnected unexpectedly",
                    ));
                }
            }
            remaining = remaining
                .checked_sub(start.elapsed())
                .unwrap_or(Duration::ZERO);
        }
        // listener dropped here; its Drop impl calls unregister() automatically.
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum AudioUnitMode {
    /// HAL Output AudioUnit with input enabled, pinned to a specific device.
    Input,
    /// HAL Output AudioUnit for output, pinned to a specific device.
    Output,
    /// DefaultOutput AudioUnit; follows the system default output device automatically.
    DefaultOutput,
}

fn audio_unit_from_device(
    device: &Device,
    mode: AudioUnitMode,
) -> Result<AudioUnit, coreaudio::Error> {
    match mode {
        AudioUnitMode::DefaultOutput => {
            AudioUnit::new_uninitialized(coreaudio::audio_unit::IOType::DefaultOutput)
        }
        AudioUnitMode::Input => {
            audio_unit_from_device_id_uninitialized(device.audio_device_id, true)
        }
        AudioUnitMode::Output => {
            // Do not use audio_unit_from_device_id_uninitialized here: that function compares the
            // device ID against the live system default and silently switches to DefaultOutput
            // mode if they match. We explicitly pin HalOutput unit here.
            let mut audio_unit =
                AudioUnit::new_uninitialized(coreaudio::audio_unit::IOType::HalOutput)?;
            // Device selection is a device-level property:
            // always use Scope::Global + Element::Output
            audio_unit.set_property(
                kAudioOutputUnitProperty_CurrentDevice,
                Scope::Global,
                Element::Output,
                Some(&device.audio_device_id),
            )?;
            Ok(audio_unit)
        }
    }
}

fn get_io_buffer_frame_size_range(device_id: AudioDeviceID) -> Result<SupportedBufferSize, Error> {
    let property_address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyBufferFrameSizeRange,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMaster,
    };
    // SAFETY: AudioObjectGetPropertyData writes exactly one AudioValueRange into the output
    // pointer when querying kAudioDevicePropertyBufferFrameSizeRange. We verify the status
    // before reading the value.
    let mut range: AudioValueRange = unsafe { mem::zeroed() };
    let mut data_size = mem::size_of::<AudioValueRange>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device_id,
            NonNull::from(&property_address),
            0,
            null(),
            NonNull::from(&mut data_size),
            NonNull::from(&mut range).cast(),
        )
    };
    check_os_status(status)?;
    Ok(SupportedBufferSize::Range {
        min: range.mMinimum as u32,
        max: range.mMaximum as u32,
    })
}

/// Compute the capture-side timestamp from a callback instant and the input latency.
///
/// Falls back to `callback` and reports a [`ErrorKind::BackendError`] via `error_callback` if
/// `callback - delay` would underflow. `callback` is a monotonic clock that starts before the
/// stream opens, so underflow indicates a pathological latency value and should not happen in
/// practice.
pub(super) fn estimate_capture_instant(
    callback: StreamInstant,
    delay: Duration,
    error_callback: &ErrorCallbackArc,
) -> StreamInstant {
    callback.checked_sub(delay).unwrap_or_else(|| {
        let _ = try_emit_error(
            error_callback,
            Error::with_message(
                ErrorKind::BackendError,
                "timestamp underflow computing capture instant",
            ),
        );
        callback
    })
}

/// Compute the playback-side timestamp from a callback instant and the output latency.
///
/// Falls back to `callback` and reports a [`ErrorKind::BackendError`] via `error_callback` if
/// `callback + delay` would overflow. The representation supports ~585 billion years of stream
/// uptime, so overflow indicates a pathological latency value and should not happen in practice.
pub(super) fn estimate_playback_instant(
    callback: StreamInstant,
    delay: Duration,
    error_callback: &ErrorCallbackArc,
) -> StreamInstant {
    callback.checked_add(delay).unwrap_or_else(|| {
        let _ = try_emit_error(
            error_callback,
            Error::with_message(
                ErrorKind::BackendError,
                "timestamp overflow computing playback instant",
            ),
        );
        callback
    })
}

impl DeviceTrait for Device {
    type SupportedInputConfigs = SupportedInputConfigs;
    type SupportedOutputConfigs = SupportedOutputConfigs;
    type Stream = Stream;

    fn description(&self) -> Result<DeviceDescription, Error> {
        Device::description(self)
    }

    fn id(&self) -> Result<DeviceId, Error> {
        Device::id(self)
    }

    fn supported_input_configs(&self) -> Result<Self::SupportedInputConfigs, Error> {
        Device::supported_input_configs(self)
    }

    fn supported_output_configs(&self) -> Result<Self::SupportedOutputConfigs, Error> {
        Device::supported_output_configs(self)
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        Device::default_input_config(self)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        Device::default_output_config(self)
    }

    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Device::build_input_stream_raw(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Device::build_output_stream_raw(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }

    fn supports_duplex(&self) -> bool {
        // Any `AudioDeviceID` that exposes both directions can be driven by a single HALOutput
        // AudioUnit, which delivers input and output to one render callback.
        //
        // For non-aggregate devices the clock is shared by construction (one piece of hardware).
        // For aggregate devices, CoreAudio drift-corrects across sub-device clocks — drift
        // correction is configured per-aggregate in Audio MIDI Setup and is enabled by default.
        // The resulting callback is sample-aligned (any drift between physical clocks is
        // absorbed by the aggregate). We trust that user-configured aggregates are what the
        // user wants and accept them here.
        self.supports_input() && self.supports_output()
    }

    fn build_duplex_stream_raw<D, E>(
        &self,
        config: DuplexStreamConfig,
        sample_format: SampleFormat,
        data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Self::Stream, Error>
    where
        D: FnMut(&Data, &mut Data, &DuplexCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        Device::build_duplex_stream_raw(
            self,
            config,
            sample_format,
            data_callback,
            error_callback,
            timeout,
        )
    }
}

#[derive(Clone)]
pub struct Device {
    pub(crate) audio_device_id: AudioDeviceID,
    pub(crate) is_default_output: bool,
}

impl Device {
    /// Construct a new device given its ID.
    /// Useful for constructing hidden devices.
    pub fn new(audio_device_id: AudioDeviceID) -> Self {
        Self {
            audio_device_id,
            is_default_output: false,
        }
    }

    /// Checks if this device is an aggregate device.
    ///
    /// Aggregate devices combine multiple physical devices into a single logical device.
    fn is_aggregate_device(&self) -> bool {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioObjectPropertyClass,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        let mut class_id: AudioClassID = 0;
        let data_size = size_of::<AudioClassID>() as u32;

        // SAFETY: AudioObjectGetPropertyData is documented to write an AudioClassID
        // for kAudioObjectPropertyClass. We check the status before using the value.
        let status = unsafe {
            AudioObjectGetPropertyData(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&data_size),
                NonNull::from(&mut class_id).cast(),
            )
        };

        // If successful, check if it's an aggregate device
        status == 0 && class_id == kAudioAggregateDeviceClassID
    }

    fn description(&self) -> Result<crate::DeviceDescription, Error> {
        let name = get_device_name(self.audio_device_id).context("Failed to get device name")?;

        let input_configs = self
            .supported_input_configs()
            .map(|configs| configs.count() as ChannelCount)
            .ok();
        let output_configs = self
            .supported_output_configs()
            .map(|configs| configs.count() as ChannelCount)
            .ok();

        let direction =
            crate::device_description::direction_from_counts(input_configs, output_configs);

        let mut builder = DeviceDescriptionBuilder::new(name).direction(direction);

        // Check if this is an aggregate device
        if self.is_aggregate_device() {
            builder = builder.interface_type(InterfaceType::Aggregate);
        }

        Ok(builder.build())
    }

    fn id(&self) -> Result<DeviceId, Error> {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyDeviceUID,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        };

        // CFString is copied from the audio object, use wrap_under_create_rule
        let mut uid: *mut CFString = std::ptr::null_mut();
        let mut data_size = size_of::<*mut CFString>() as u32;

        // SAFETY: AudioObjectGetPropertyData is documented to write a CFString pointer
        // for kAudioDevicePropertyDeviceUID. We check the status code before use.
        let status = unsafe {
            AudioObjectGetPropertyData(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
                NonNull::from(&mut uid).cast(),
            )
        };
        check_os_status(status)?;

        // SAFETY: Status was successful, meaning the API call succeeded.
        // We now check if the returned uid is non-null before use.
        if !uid.is_null() {
            let uid_string = unsafe { CFString::wrap_under_create_rule(uid).to_string() };
            Ok(DeviceId::new(
                crate::platform::HostId::CoreAudio,
                uid_string,
            ))
        } else {
            Err(ErrorKind::DeviceNotAvailable.into())
        }
    }

    // Logic re-used between `supported_input_configs` and `supported_output_configs`.
    #[allow(clippy::cast_ptr_alignment)]
    fn supported_configs(
        &self,
        scope: AudioObjectPropertyScope,
    ) -> Result<SupportedOutputConfigs, Error> {
        let mut property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreamConfiguration,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMaster,
        };

        unsafe {
            // Retrieve the devices audio buffer list.
            let mut data_size = 0u32;
            let status = AudioObjectGetPropertyDataSize(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
            );
            check_os_status(status)?;

            let mut audio_buffer_list: Vec<u8> = vec![];
            audio_buffer_list.reserve_exact(data_size as usize);
            let status = AudioObjectGetPropertyData(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
                NonNull::new(audio_buffer_list.as_mut_ptr()).unwrap().cast(),
            );
            check_os_status(status)?;

            let audio_buffer_list = audio_buffer_list.as_mut_ptr() as *mut AudioBufferList;

            // Read the number of buffers without assuming alignment (avoid UB).
            let nb_ptr = core::ptr::addr_of!((*audio_buffer_list).mNumberBuffers);
            let n_buffers = core::ptr::read_unaligned(nb_ptr) as usize;
            // If there are no buffers, skip.
            if n_buffers == 0 {
                return Ok(vec![].into_iter());
            }

            // Count the number of channels as the sum of all channels in all output buffers.
            let first_buf_ptr =
                core::ptr::addr_of!((*audio_buffer_list).mBuffers) as *const AudioBuffer;
            let mut n_channels = 0usize;
            for i in 0..n_buffers {
                let buf_ptr = first_buf_ptr.add(i);
                // Read potentially unaligned
                let buf: AudioBuffer = core::ptr::read_unaligned(buf_ptr);
                n_channels += buf.mNumberChannels as usize;
            }

            // TODO: macOS should support U8, I16, I32, F32 and F64. This should allow for using
            // I16 but just use F32 for now as it's the default anyway.
            let sample_format = SampleFormat::F32;

            // Get available sample rate ranges.
            // The property "kAudioDevicePropertyAvailableNominalSampleRates" returns a list of pairs of
            // minimum and maximum sample rates but most of the devices returns pairs of same values though the underlying mechanism is unclear.
            // This may cause issues when, for example, sorting the configs by the sample rates.
            // We follows the implementation of RtAudio, which returns single element of config
            // when all the pairs have the same values and returns multiple elements otherwise.
            // See https://github.com/thestk/rtaudio/blob/master/RtAudio.cpp#L1369C1-L1375C39

            property_address.mSelector = kAudioDevicePropertyAvailableNominalSampleRates;
            let mut data_size = 0u32;
            let status = AudioObjectGetPropertyDataSize(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
            );
            check_os_status(status)?;

            let n_ranges = data_size as usize / mem::size_of::<AudioValueRange>();
            let mut ranges: Vec<AudioValueRange> = Vec::with_capacity(n_ranges);
            let status = AudioObjectGetPropertyData(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
                NonNull::new(ranges.as_mut_ptr()).unwrap().cast(),
            );
            check_os_status(status)?;

            ranges.set_len(n_ranges);

            #[allow(non_upper_case_globals)]
            match scope {
                kAudioObjectPropertyScopeInput | kAudioObjectPropertyScopeOutput => {}
                _ => {
                    return Err(Error::with_message(
                        ErrorKind::UnsupportedOperation,
                        "Unexpected audio property scope",
                    ))
                }
            }
            let buffer_size = get_io_buffer_frame_size_range(self.audio_device_id)?;

            // Most hardware reports discrete rates (mMinimum == mMaximum); some aggregate or
            // virtual devices report continuous ranges.
            let fmts: Vec<_> = ranges
                .iter()
                .map(|range| SupportedStreamConfigRange {
                    channels: n_channels as ChannelCount,
                    min_sample_rate: range.mMinimum as u32,
                    max_sample_rate: range.mMaximum as u32,
                    buffer_size,
                    sample_format,
                })
                .collect();
            Ok(fmts.into_iter())
        }
    }

    fn supported_input_configs(&self) -> Result<SupportedOutputConfigs, Error> {
        self.supported_configs(kAudioObjectPropertyScopeInput)
    }

    fn supported_output_configs(&self) -> Result<SupportedOutputConfigs, Error> {
        self.supported_configs(kAudioObjectPropertyScopeOutput)
    }

    fn default_config(
        &self,
        scope: AudioObjectPropertyScope,
    ) -> Result<SupportedStreamConfig, Error> {
        let property_address = AudioObjectPropertyAddress {
            mSelector: kAudioDevicePropertyStreamFormat,
            mScope: scope,
            mElement: kAudioObjectPropertyElementMaster,
        };

        unsafe {
            let mut asbd: AudioStreamBasicDescription = mem::zeroed();
            let mut data_size = mem::size_of::<AudioStreamBasicDescription>() as u32;
            let status = AudioObjectGetPropertyData(
                self.audio_device_id,
                NonNull::from(&property_address),
                0,
                null(),
                NonNull::from(&mut data_size),
                NonNull::from(&mut asbd).cast(),
            );
            check_os_status(status)?;

            let sample_format = {
                let audio_format = coreaudio::audio_unit::AudioFormat::from_format_and_flag(
                    asbd.mFormatID,
                    Some(asbd.mFormatFlags),
                );
                let flags = match audio_format {
                    Some(coreaudio::audio_unit::AudioFormat::LinearPCM(flags)) => flags,
                    _ => {
                        return Err(Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Audio format is not linear PCM",
                        ))
                    }
                };
                let maybe_sample_format =
                    coreaudio::audio_unit::SampleFormat::from_flags_and_bits_per_sample(
                        flags,
                        asbd.mBitsPerChannel,
                    );
                match maybe_sample_format {
                    Some(coreaudio::audio_unit::SampleFormat::F32) => SampleFormat::F32,
                    Some(coreaudio::audio_unit::SampleFormat::I16) => SampleFormat::I16,
                    _ => {
                        return Err(Error::with_message(
                            ErrorKind::UnsupportedConfig,
                            "Sample format is not supported; supported formats are F32 and I16",
                        ))
                    }
                }
            };

            #[allow(non_upper_case_globals)]
            match scope {
                kAudioObjectPropertyScopeInput | kAudioObjectPropertyScopeOutput => {}
                _ => {
                    return Err(Error::with_message(
                        ErrorKind::UnsupportedOperation,
                        "Unexpected audio property scope",
                    ))
                }
            }
            let buffer_size = get_io_buffer_frame_size_range(self.audio_device_id)?;

            let config = SupportedStreamConfig {
                sample_rate: asbd.mSampleRate as _,
                channels: asbd.mChannelsPerFrame as _,
                buffer_size,
                sample_format,
            };
            Ok(config)
        }
    }

    fn default_input_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.default_config(kAudioObjectPropertyScopeInput)
    }

    fn default_output_config(&self) -> Result<SupportedStreamConfig, Error> {
        self.default_config(kAudioObjectPropertyScopeOutput)
    }

    /// Check if this device supports input (recording).
    fn supports_input(&self) -> bool {
        // Check if the device has input channels by trying to get its input configuration
        self.supported_input_configs()
            .map(|mut configs| configs.next().is_some())
            .unwrap_or(false)
    }
}

impl Device {
    #[allow(clippy::cast_ptr_alignment)]
    #[allow(clippy::while_immutable_condition)]
    #[allow(clippy::float_cmp)]
    fn build_input_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        mut data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Stream, Error>
    where
        D: FnMut(&Data, &InputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        crate::validate_stream_config(&config)?;
        // The scope and element for working with a device's input stream.
        let scope = Scope::Output;
        let element = Element::Input;

        // Set the physical stream format (bit depth + sample rate) on the hardware device.
        // This avoids unnecessary format conversions, which is especially important on aggregate
        // devices. Falls back to sample-rate-only if no matching physical format is available.
        if set_physical_format(
            self.audio_device_id,
            config.sample_rate,
            config.channels,
            sample_format,
        )
        .is_err()
        {
            set_sample_rate(self.audio_device_id, config.sample_rate, timeout)?;
        }

        let mut loopback_aggregate: Option<LoopbackDevice> = None;
        let mut audio_unit = if self.supports_input() {
            audio_unit_from_device(self, AudioUnitMode::Input)?
        } else {
            loopback_aggregate.replace(LoopbackDevice::from_device(self)?);
            audio_unit_from_device(
                &loopback_aggregate.as_ref().unwrap().aggregate_device,
                AudioUnitMode::Input,
            )?
        };

        // Configure stream format and buffer size for predictable callback behavior.
        let effective_device_id = loopback_aggregate
            .as_ref()
            .map(|l| l.aggregate_device.audio_device_id)
            .unwrap_or(self.audio_device_id);
        configure_stream_format_and_buffer(
            &mut audio_unit,
            config,
            sample_format,
            scope,
            element,
            effective_device_id,
        )?;

        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let error_callback_disconnect = error_callback.clone();

        // Register the callback that is being called by coreaudio whenever it needs data to be
        // fed to the audio buffer.
        let (bytes_per_channel, sample_rate, device_buffer_frames, extra_latency_frames) =
            setup_callback_vars(&audio_unit, config, sample_format, Scope::Input);

        type Args = render_callback::Args<data::Raw>;
        audio_unit.set_input_callback(move |args: Args| unsafe {
            // SAFETY: We configure the stream format as interleaved (via asbd_from_config which
            // does not set kAudioFormatFlagIsNonInterleaved). Interleaved format always has
            // exactly one buffer containing all channels, so mBuffers[0] is always valid.
            let AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: data_byte_size,
                mData: data,
            } = (*args.data.data).mBuffers[0];

            let data = data as *mut ();
            let len = data_byte_size as usize / bytes_per_channel;
            let data = Data::from_parts(data, len, sample_format);

            let callback = match host_time_to_stream_instant(args.time_stamp.mHostTime) {
                Err(err) => {
                    let _ = try_emit_error(&error_callback, err);
                    return Err(());
                }
                Ok(cb) => cb,
            };
            let buffer_frames = len / channels as usize;
            let latency_frames =
                device_buffer_frames.unwrap_or(buffer_frames) + extra_latency_frames;
            let delay = frames_to_duration(latency_frames as FrameCount, sample_rate);
            let capture = estimate_capture_instant(callback, delay, &error_callback);
            let timestamp = InputStreamTimestamp { callback, capture };

            let info = InputCallbackInfo { timestamp };
            data_callback(&data, &info);
            Ok(())
        })?;

        // All properties and callbacks are now configured on the uninitialized unit.
        // Initialize here so CoreAudio allocates its internal buffers for the actual format.
        audio_unit.initialize()?;

        let inner_arc = Arc::new(Mutex::new(StreamInner {
            playing: false,
            audio_unit: ManuallyDrop::new(audio_unit),
            _device_id: self.audio_device_id,
            _loopback_device: loopback_aggregate,
            duplex_callback_ptr: None,
        }));
        let weak_inner = Arc::downgrade(&inner_arc);
        let monitor: Box<dyn Monitor> = Box::new(DisconnectManager::new(
            self.audio_device_id,
            weak_inner,
            error_callback_disconnect,
            false,
        )?);
        let stream = Stream::new(inner_arc, monitor);
        stream.signal_ready();
        Ok(stream)
    }

    fn build_output_stream_raw<D, E>(
        &self,
        config: StreamConfig,
        sample_format: SampleFormat,
        mut data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Stream, Error>
    where
        D: FnMut(&mut Data, &OutputCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        crate::validate_stream_config(&config)?;
        // Best-effort: set the physical stream format (bit depth + sample rate) on the hardware.
        // This avoids unnecessary conversions, especially on aggregate devices. Not an error if
        // it fails — the AudioUnit will handle format conversion as before.
        if set_physical_format(
            self.audio_device_id,
            config.sample_rate,
            config.channels,
            sample_format,
        )
        .is_err()
        {
            set_sample_rate(self.audio_device_id, config.sample_rate, timeout)?;
        }

        let mode = if self.is_default_output {
            AudioUnitMode::DefaultOutput
        } else {
            AudioUnitMode::Output
        };
        let mut audio_unit = audio_unit_from_device(self, mode)?;

        // The scope and element for working with a device's output stream.
        let scope = Scope::Input;
        let element = Element::Output;

        // Configure device buffer (see comprehensive documentation in input stream above)
        configure_stream_format_and_buffer(
            &mut audio_unit,
            config,
            sample_format,
            scope,
            element,
            self.audio_device_id,
        )?;

        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let error_callback_for_render = error_callback.clone();

        // Register the callback that is being called by coreaudio whenever it needs data to be
        // fed to the audio buffer.
        let (bytes_per_channel, sample_rate, device_buffer_frames, extra_latency_frames) =
            setup_callback_vars(&audio_unit, config, sample_format, Scope::Output);

        type Args = render_callback::Args<data::Raw>;
        audio_unit.set_render_callback(move |args: Args| unsafe {
            // SAFETY: We configure the stream format as interleaved (via asbd_from_config which
            // does not set kAudioFormatFlagIsNonInterleaved). Interleaved format always has
            // exactly one buffer containing all channels, so mBuffers[0] is always valid.
            let AudioBuffer {
                mNumberChannels: channels,
                mDataByteSize: data_byte_size,
                mData: data,
            } = (*args.data.data).mBuffers[0];

            let data = data as *mut ();
            let len = data_byte_size as usize / bytes_per_channel;
            let mut data = Data::from_parts(data, len, sample_format);

            let callback = match host_time_to_stream_instant(args.time_stamp.mHostTime) {
                Err(err) => {
                    let _ = try_emit_error(&error_callback_for_render, err);
                    return Err(());
                }
                Ok(cb) => cb,
            };
            let buffer_frames = len / channels as usize;
            // Use device buffer size for latency calculation if available
            let latency_frames =
                device_buffer_frames.unwrap_or(buffer_frames) + extra_latency_frames;
            let delay = frames_to_duration(latency_frames as FrameCount, sample_rate);
            let playback = estimate_playback_instant(callback, delay, &error_callback_for_render);
            let timestamp = OutputStreamTimestamp { callback, playback };

            let info = OutputCallbackInfo { timestamp };
            data_callback(&mut data, &info);
            Ok(())
        })?;

        // All properties and callbacks are now configured on the uninitialized unit.
        // Initialize here so CoreAudio allocates its internal buffers for the actual format.
        audio_unit.initialize()?;

        let inner_arc = Arc::new(Mutex::new(StreamInner {
            playing: false,
            audio_unit: ManuallyDrop::new(audio_unit),
            _device_id: self.audio_device_id,
            _loopback_device: None,
            duplex_callback_ptr: None,
        }));
        let weak_inner = Arc::downgrade(&inner_arc);
        let monitor: Box<dyn Monitor> = if matches!(mode, AudioUnitMode::DefaultOutput) {
            Box::new(DefaultOutputMonitor::new(weak_inner, error_callback)?)
        } else {
            Box::new(DisconnectManager::new(
                self.audio_device_id,
                weak_inner,
                error_callback,
                false,
            )?)
        };
        let stream = Stream::new(inner_arc, monitor);
        stream.signal_ready();
        Ok(stream)
    }

    /// Build a synchronized duplex stream on a single HALOutput AudioUnit.
    ///
    /// Both directions share the same hardware callback so input and output are sample-aligned.
    /// `coreaudio-rs` does not expose a builder for this case, so we drive the AudioUnit setup
    /// directly here and register the render callback via [`duplex_input_proc`].
    pub(super) fn build_duplex_stream_raw<D, E>(
        &self,
        config: DuplexStreamConfig,
        sample_format: SampleFormat,
        mut data_callback: D,
        error_callback: E,
        timeout: Option<Duration>,
    ) -> Result<Stream, Error>
    where
        D: FnMut(&Data, &mut Data, &DuplexCallbackInfo) + Send + 'static,
        E: FnMut(Error) + Send + 'static,
    {
        if !(self.supports_input() && self.supports_output()) {
            return Err(Error::with_message(
                ErrorKind::UnsupportedOperation,
                "device does not support both input and output",
            ));
        }

        set_sample_rate(self.audio_device_id, config.sample_rate, timeout)?;

        // HALOutput with both directions enabled. We pin the device explicitly rather than
        // following the system default — duplex callers care about which device they are on.
        let mut audio_unit =
            AudioUnit::new_uninitialized(coreaudio::audio_unit::IOType::HalOutput)?;

        // Enable IO on both buses. `kAudioOutputUnitProperty_EnableIO` is a 0/1 toggle.
        const ENABLED: u32 = 1;
        audio_unit.set_property(
            kAudioOutputUnitProperty_EnableIO,
            Scope::Input,
            Element::Input,
            Some(&ENABLED),
        )?;
        audio_unit.set_property(
            kAudioOutputUnitProperty_EnableIO,
            Scope::Output,
            Element::Output,
            Some(&ENABLED),
        )?;

        // Pin the device.
        audio_unit.set_property(
            kAudioOutputUnitProperty_CurrentDevice,
            Scope::Global,
            Element::Output,
            Some(&self.audio_device_id),
        )?;

        // Client-side format. Note the inverted scopes — the input bus's *output* side is what
        // the client reads; the output bus's *input* side is what the client writes. Easy to get
        // backwards.
        let input_config = StreamConfig {
            channels: config.input_channels,
            sample_rate: config.sample_rate,
            buffer_size: config.buffer_size,
        };
        let output_config = StreamConfig {
            channels: config.output_channels,
            sample_rate: config.sample_rate,
            buffer_size: config.buffer_size,
        };
        let input_asbd = asbd_from_config(input_config, sample_format);
        audio_unit.set_property(
            kAudioUnitProperty_StreamFormat,
            Scope::Output,
            Element::Input,
            Some(&input_asbd),
        )?;
        let output_asbd = asbd_from_config(output_config, sample_format);
        audio_unit.set_property(
            kAudioUnitProperty_StreamFormat,
            Scope::Input,
            Element::Output,
            Some(&output_asbd),
        )?;

        // Apply the requested fixed buffer size (device-level property).
        if let BufferSize::Fixed(frames) = config.buffer_size {
            audio_unit.set_property(
                kAudioDevicePropertyBufferFrameSize,
                Scope::Global,
                Element::Output,
                Some(&frames),
            )?;
        }

        audio_unit.initialize()?;

        // Snapshot of the values the audio callback needs without holding the AudioUnit.
        let sample_rate = config.sample_rate;
        let device_buffer_frames = get_device_buffer_frame_size(&audio_unit).map_err(|e| {
            Error::with_message(
                ErrorKind::BackendError,
                format!("failed to query device buffer size: {e}"),
            )
        })?;
        let input_channels = config.input_channels as usize;
        let sample_bytes = sample_format.sample_size();
        let input_buffer_bytes = device_buffer_frames * input_channels * sample_bytes;
        let mut input_buffer: Box<[u8]> = vec![0u8; input_buffer_bytes].into_boxed_slice();

        // Raw AudioUnit handle for `AudioUnitRender` calls from inside the closure. The
        // pointer (`*mut OpaqueAudioComponentInstance`) isn't `Send`, but `DuplexProcFn` does
        // not require `Send` either — Send-ness is asserted at the `DuplexProcWrapper` level,
        // see its safety doc.
        let raw_audio_unit = *audio_unit.as_ref();

        let error_callback: ErrorCallbackArc = Arc::new(Mutex::new(error_callback));
        let error_callback_for_callback = error_callback.clone();

        // Once tripped, every subsequent callback bails. The `DisconnectManager` (configured
        // below with `listen_buffer_size = true`) also fires `StreamInvalidated` on the
        // property listener thread; this is the race guard for callbacks that fire *before*
        // the manager pauses us.
        let buffer_size_changed = std::sync::atomic::AtomicBool::new(false);

        let duplex_proc: Box<super::duplex::DuplexProcFn> = Box::new(
            move |io_action_flags: NonNull<AudioUnitRenderActionFlags>,
                  in_time_stamp: NonNull<AudioTimeStamp>,
                  _in_bus_number: u32,
                  in_number_frames: u32,
                  io_data: *mut AudioBufferList|
                  -> i32 {
                use std::sync::atomic::Ordering;

                if buffer_size_changed.load(Ordering::Relaxed) {
                    return kAudio_ParamError;
                }

                if io_data.is_null() {
                    return kAudio_ParamError;
                }
                // SAFETY: io_data is non-null per the check above; CoreAudio guarantees
                // validity for the callback duration.
                let buffer_list = unsafe { &mut *io_data };
                if buffer_list.mNumberBuffers == 0 {
                    return kAudio_ParamError;
                }

                let num_frames = in_number_frames as usize;
                let input_samples = num_frames * input_channels;
                let input_bytes = input_samples * sample_bytes;
                if input_bytes != input_buffer.len() {
                    buffer_size_changed.store(true, Ordering::Relaxed);
                    return kAudio_ParamError;
                }

                // SAFETY: in_time_stamp is valid per the CoreAudio callback contract.
                let timestamp: &AudioTimeStamp = unsafe { in_time_stamp.as_ref() };
                let callback = match host_time_to_stream_instant(timestamp.mHostTime) {
                    Ok(t) => t,
                    Err(err) => {
                        let _ = try_emit_error(&error_callback_for_callback, err);
                        return kAudio_ParamError;
                    }
                };
                let delay = frames_to_duration(device_buffer_frames as FrameCount, sample_rate);
                let capture =
                    estimate_capture_instant(callback, delay, &error_callback_for_callback);
                let playback =
                    estimate_playback_instant(callback, delay, &error_callback_for_callback);
                let input_timestamp = InputStreamTimestamp { callback, capture };
                let output_timestamp = OutputStreamTimestamp { callback, playback };

                // Output side: write directly into CoreAudio's first buffer.
                let output_buf = &mut buffer_list.mBuffers[0];
                if output_buf.mData.is_null() {
                    return kAudio_ParamError;
                }
                let output_samples = output_buf.mDataByteSize as usize / sample_bytes;
                // SAFETY: output_buf.mData is non-null per the check above and points to a
                // buffer of mDataByteSize bytes for the callback duration.
                let mut output_data = unsafe {
                    Data::from_parts(output_buf.mData as *mut (), output_samples, sample_format)
                };

                // Input side: pull from the input bus into our scratch buffer.
                let mut input_buffer_list = AudioBufferList {
                    mNumberBuffers: 1,
                    mBuffers: [AudioBuffer {
                        mNumberChannels: input_channels as u32,
                        mDataByteSize: input_bytes as u32,
                        mData: input_buffer.as_mut_ptr() as *mut c_void,
                    }],
                };
                // SAFETY: raw_audio_unit is valid for the callback's lifetime;
                // input_buffer_list and input_buffer are alive on the stack/heap here.
                let status = unsafe {
                    AudioUnitRender(
                        raw_audio_unit,
                        io_action_flags.as_ptr(),
                        in_time_stamp,
                        1, // element 1 == input bus
                        in_number_frames,
                        NonNull::new_unchecked(&mut input_buffer_list),
                    )
                };
                if status != 0 {
                    let _ = try_emit_error(
                        &error_callback_for_callback,
                        Error::with_message(
                            ErrorKind::BackendError,
                            format!("AudioUnitRender (input) returned OSStatus {status}"),
                        ),
                    );
                    // Zero the buffer so the user callback sees silence rather than stale data.
                    input_buffer[..input_bytes].fill(0);
                }

                // SAFETY: input_buffer is bounds-checked, was just filled (or zeroed) by
                // `AudioUnitRender`, and outlives this `Data` reference (it's owned by the
                // closure, which outlives this invocation).
                let input_data = unsafe {
                    Data::from_parts(
                        input_buffer.as_mut_ptr() as *mut (),
                        input_samples,
                        sample_format,
                    )
                };

                let info = DuplexCallbackInfo::new(input_timestamp, output_timestamp);
                data_callback(&input_data, &mut output_data, &info);

                0
            },
        );

        // Box up the wrapper and hand the raw pointer to CoreAudio. Reclaimed in `Drop for
        // StreamInner` after the audio unit is stopped.
        let wrapper = Box::new(DuplexProcWrapper {
            callback: duplex_proc,
        });
        let wrapper_ptr = Box::into_raw(wrapper);

        let render_callback = AURenderCallbackStruct {
            inputProc: Some(duplex_input_proc),
            inputProcRefCon: wrapper_ptr as *mut c_void,
        };
        audio_unit.set_property(
            kAudioUnitProperty_SetRenderCallback,
            Scope::Global,
            Element::Output,
            Some(&render_callback),
        )?;

        let inner_arc = Arc::new(Mutex::new(StreamInner {
            playing: false,
            audio_unit: ManuallyDrop::new(audio_unit),
            _device_id: self.audio_device_id,
            _loopback_device: None,
            duplex_callback_ptr: Some(DuplexCallbackPtr(wrapper_ptr)),
        }));
        let weak_inner = Arc::downgrade(&inner_arc);
        let monitor: Box<dyn Monitor> = Box::new(DisconnectManager::new(
            self.audio_device_id,
            weak_inner,
            error_callback,
            true,
        )?);

        // Start the audio unit. The user calls `Stream::play` to begin processing later, but
        // following the existing input/output paths we start the unit here while still in the
        // build function so failures surface synchronously.
        inner_arc
            .lock()
            .map_err(|_| Error::with_message(ErrorKind::StreamInvalidated, "Stream lock poisoned"))?
            .play()?;

        let stream = Stream::new(inner_arc, monitor);
        stream.signal_ready();
        Ok(stream)
    }
}

impl PartialEq for Device {
    fn eq(&self, other: &Self) -> bool {
        self.audio_device_id == other.audio_device_id
    }
}

impl Eq for Device {}

impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let desc = self.description().map_err(|_| fmt::Error)?;
        f.write_str(desc.name())
    }
}

impl std::hash::Hash for Device {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.audio_device_id.hash(state);
    }
}

impl fmt::Debug for Device {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("Device")
            .field("audio_device_id", &self.audio_device_id)
            .field("name", &self.description().map(|d| d.name().to_owned()))
            .finish()
    }
}

/// Configure stream format and buffer size for CoreAudio stream.
///
/// This handles the common setup tasks for both input and output streams:
/// - Sets the stream format (ASBD)
/// - Configures buffer size for Fixed buffer size requests
fn configure_stream_format_and_buffer(
    audio_unit: &mut AudioUnit,
    config: StreamConfig,
    sample_format: SampleFormat,
    scope: Scope,
    element: Element,
    device_id: AudioDeviceID,
) -> Result<(), Error> {
    // Set the stream format using stream-specific scope/element
    // - Input streams: scope=Output, element=Input (configuring output format of input element)
    // - Output streams: scope=Input, element=Output (configuring input format of output element)
    let asbd = asbd_from_config(config, sample_format);
    audio_unit.set_property(kAudioUnitProperty_StreamFormat, scope, element, Some(&asbd))?;

    // Configure device buffer size if requested
    if let BufferSize::Fixed(buffer_size) = config.buffer_size {
        // Pre-validate against the hardware range so callers get a human-readable error.
        if let Ok(SupportedBufferSize::Range { min, max }) =
            get_io_buffer_frame_size_range(device_id)
        {
            if !(min..=max).contains(&buffer_size) {
                return Err(Error::with_message(
                    ErrorKind::UnsupportedConfig,
                    format!(
                        "Buffer size {buffer_size} is not in the supported range {min}..={max}"
                    ),
                ));
            }
        }
        // IMPORTANT: Buffer frame size is a DEVICE-LEVEL property, not stream-specific.
        // Unlike stream format above, we ALWAYS use Scope::Global + Element::Output
        // for device properties, regardless of whether this is an input or output stream.
        // This is consistent with other device properties like:
        // - kAudioOutputUnitProperty_CurrentDevice
        // - kAudioDevicePropertyBufferFrameSizeRange
        // The Element::Output here doesn't mean "output stream only" - it's the
        // canonical element used for device-wide properties in Core Audio.
        audio_unit.set_property(
            kAudioDevicePropertyBufferFrameSize,
            Scope::Global,
            Element::Output,
            Some(&buffer_size),
        )?;
    }

    Ok(())
}

/// Returns the sum of the device latency and safety offset in frames.
fn get_device_extra_latency_frames(audio_unit: &AudioUnit, scope: Scope) -> usize {
    let device_latency: u32 = audio_unit
        .get_property(kAudioDevicePropertyLatency, scope, Element::Output)
        .unwrap_or(0);
    let safety_offset: u32 = audio_unit
        .get_property(kAudioDevicePropertySafetyOffset, scope, Element::Output)
        .unwrap_or(0);
    (device_latency + safety_offset) as usize
}

/// Setup common callback variables, querying both the I/O buffer size and extra hardware latency.
///
/// Returns `(bytes_per_channel, sample_rate, device_buffer_frames, extra_latency_frames)`
fn setup_callback_vars(
    audio_unit: &AudioUnit,
    config: StreamConfig,
    sample_format: SampleFormat,
    scope: Scope,
) -> (usize, SampleRate, Option<usize>, usize) {
    let bytes_per_channel = sample_format.sample_size();
    let sample_rate = config.sample_rate;

    let device_buffer_frames = get_device_buffer_frame_size(audio_unit).ok();
    let extra_latency_frames = get_device_extra_latency_frames(audio_unit, scope);

    (
        bytes_per_channel,
        sample_rate,
        device_buffer_frames,
        extra_latency_frames,
    )
}

/// Query the current device buffer frame size from CoreAudio.
///
/// Buffer frame size is a device-level property that always uses Scope::Global + Element::Output,
/// regardless of whether the audio unit is configured for input or output streams.
pub(crate) fn get_device_buffer_frame_size(
    audio_unit: &AudioUnit,
) -> Result<usize, coreaudio::Error> {
    // Device-level property: always use Scope::Global + Element::Output
    // This is consistent with how we set the buffer size and query the buffer size range
    let frames: u32 = audio_unit.get_property(
        kAudioDevicePropertyBufferFrameSize,
        Scope::Global,
        Element::Output,
    )?;
    Ok(frames as usize)
}
