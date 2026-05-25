#![allow(deprecated)]
use std::mem::ManuallyDrop;
use std::sync::{mpsc, Arc, Mutex, Weak};

use coreaudio::audio_unit::AudioUnit;
use objc2_core_audio::{
    kAudioDevicePropertyBufferFrameSize, kAudioDevicePropertyDeviceIsAlive,
    kAudioDevicePropertyNominalSampleRate, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, AudioDeviceID, AudioObjectPropertyAddress,
};
use property_listener::AudioObjectPropertyListener;

pub use self::enumerate::{default_input_device, default_output_device, Devices};
use super::{asbd_from_config, check_os_status, host_time_to_stream_instant, OSStatus};
use crate::{
    host::coreaudio::macos::loopback::LoopbackDevice,
    traits::{HostTrait, StreamTrait},
    Error, ErrorKind, FrameCount, ResultExt, StreamInstant,
};

mod device;
mod duplex;
pub mod enumerate;
mod loopback;
mod property_listener;
pub use device::Device;

/// Coreaudio host, the default host on macOS.
#[derive(Debug)]
pub struct Host;

impl Host {
    pub fn new() -> Result<Self, Error> {
        Ok(Host)
    }
}

impl HostTrait for Host {
    type Devices = Devices;
    type Device = Device;

    fn is_available() -> bool {
        // Assume coreaudio is always available
        true
    }

    fn devices(&self) -> Result<Self::Devices, Error> {
        Devices::new()
    }

    fn default_input_device(&self) -> Option<Self::Device> {
        default_input_device()
    }

    fn default_output_device(&self) -> Option<Self::Device> {
        default_output_device()
    }
}

/// Type alias for the error callback to reduce complexity
type ErrorCallback = Box<dyn FnMut(Error) + Send + 'static>;

/// Invoke error callback, recovering from poisoned mutex if needed.
/// Returns true if callback was invoked, false if skipped due to WouldBlock.
#[inline]
fn invoke_error_callback<E>(error_callback: &Arc<Mutex<E>>, err: Error) -> bool
where
    E: FnMut(Error) + Send,
{
    match error_callback.try_lock() {
        Ok(mut cb) => {
            cb(err);
            true
        }
        Err(std::sync::TryLockError::Poisoned(guard)) => {
            // Recover from poisoned lock to still report this error
            guard.into_inner()(err);
            true
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            // Skip if callback is busy
            false
        }
    }
}

/// Manages device disconnection listener on a dedicated thread to ensure the
/// AudioObjectPropertyListener is always created and dropped on the same thread.
/// This avoids potential threading issues with CoreAudio APIs.
///
/// When a device disconnects, this manager:
/// 1. Attempts to pause the stream to stop audio I/O
/// 2. Calls the error callback with `ErrorKind::DeviceNotAvailable`
///
/// The dedicated thread architecture ensures `Stream` can implement `Send`.
struct DisconnectManager {
    _shutdown_tx: mpsc::Sender<()>,
}

impl DisconnectManager {
    /// Create a new DisconnectManager that monitors device disconnection on a dedicated thread
    fn new(
        device_id: AudioDeviceID,
        stream_weak: Weak<Mutex<StreamInner>>,
        error_callback: Arc<Mutex<ErrorCallback>>,
        listen_buffer_size: bool,
    ) -> Result<Self, Error> {
        let (shutdown_tx, shutdown_rx) = mpsc::channel();
        let (disconnect_tx, disconnect_rx) = mpsc::channel::<Error>();
        let (ready_tx, ready_rx) = mpsc::channel();

        // Spawn a dedicated thread to own all listeners. CoreAudio requires that
        // AudioObjectPropertyListeners are added and removed on the same thread.
        let disconnect_tx_alive = disconnect_tx.clone();
        let disconnect_tx_rate = disconnect_tx.clone();
        let disconnect_tx_buffer = disconnect_tx;
        std::thread::spawn(move || {
            let alive_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyDeviceIsAlive,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let alive_listener =
                AudioObjectPropertyListener::new(device_id, alive_address, move || {
                    let _ = disconnect_tx_alive.send(Error::with_message(
                        ErrorKind::DeviceNotAvailable,
                        "device disconnected",
                    ));
                });

            let rate_address = AudioObjectPropertyAddress {
                mSelector: kAudioDevicePropertyNominalSampleRate,
                mScope: kAudioObjectPropertyScopeGlobal,
                mElement: kAudioObjectPropertyElementMain,
            };
            let rate_listener =
                AudioObjectPropertyListener::new(device_id, rate_address, move || {
                    let _ = disconnect_tx_rate.send(Error::with_message(
                        ErrorKind::StreamInvalidated,
                        "device sample rate changed",
                    ));
                });

            let buffer_size_listener = if listen_buffer_size {
                let buffer_size_address = AudioObjectPropertyAddress {
                    mSelector: kAudioDevicePropertyBufferFrameSize,
                    mScope: kAudioObjectPropertyScopeGlobal,
                    mElement: kAudioObjectPropertyElementMain,
                };
                match AudioObjectPropertyListener::new(device_id, buffer_size_address, move || {
                    let _ = disconnect_tx_buffer.send(Error::with_message(
                        ErrorKind::StreamInvalidated,
                        "device buffer size changed",
                    ));
                }) {
                    Ok(listener) => Some(listener),
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                }
            } else {
                None
            };

            match (alive_listener, rate_listener) {
                (Ok(_alive), Ok(_rate)) => {
                    let _buffer_size = buffer_size_listener;
                    let _ = ready_tx.send(Ok(()));
                    // Block until the stream is dropped; listeners are removed on drop.
                    let _ = shutdown_rx.recv();
                }
                (Err(e), _) | (_, Err(e)) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        });

        // Wait for listener creation to complete or fail
        ready_rx.recv().map_err(|_| {
            Error::with_message(
                ErrorKind::StreamInvalidated,
                "disconnect listener thread terminated unexpectedly",
            )
        })??;

        // Handle events on a separate thread
        let stream_weak_clone = stream_weak.clone();
        let error_callback_clone = error_callback.clone();
        std::thread::spawn(move || {
            while let Ok(err) = disconnect_rx.recv() {
                if let Some(stream_arc) = stream_weak_clone.upgrade() {
                    if let Ok(mut stream_inner) = stream_arc.try_lock() {
                        let _ = stream_inner.pause();
                    }
                    invoke_error_callback(&error_callback_clone, err);
                } else {
                    break;
                }
            }
        });

        Ok(DisconnectManager {
            _shutdown_tx: shutdown_tx,
        })
    }
}

/// Owned pointer to the duplex callback wrapper that is safe to send across threads.
///
/// SAFETY: The pointer is created via `Box::into_raw` on the build thread and shared with
/// CoreAudio via `inputProcRefCon`. CoreAudio dereferences it on every render callback on
/// its single-threaded audio thread for the lifetime of the stream. On drop, the audio unit
/// is stopped before reclaiming the `Box`, preventing use-after-free. `Send` is sound because
/// there is no concurrent mutable access—the build/drop thread never accesses the pointer
/// while the audio unit is running, and only reclaims it after stopping the audio unit.
struct DuplexCallbackPtr(*mut duplex::DuplexProcWrapper);

// SAFETY: See above — the pointer is shared with CoreAudio's audio thread but never
// accessed concurrently. The audio unit is stopped before reclaiming in drop.
unsafe impl Send for DuplexCallbackPtr {}

struct StreamInner {
    playing: bool,
    audio_unit: ManuallyDrop<AudioUnit>,
    // Track the device with which the audio unit was spawned.
    //
    // We must do this so that we can avoid changing the device sample rate if there is already
    // a stream associated with the device.
    #[allow(dead_code)]
    device_id: AudioDeviceID,
    /// Manage the lifetime of the aggregate device used
    /// for loopback recording
    _loopback_device: Option<LoopbackDevice>,
    /// Pointer to the duplex callback wrapper, manually managed for duplex streams.
    ///
    /// coreaudio-rs doesn't support duplex streams (enabling both input and output
    /// simultaneously), so we cannot use its `set_render_callback` API which would
    /// manage the callback lifetime automatically. Instead, we manually manage this
    /// callback pointer (created via `Box::into_raw`) and clean it up in Drop.
    ///
    /// This is None for regular input/output streams.
    duplex_callback_ptr: Option<DuplexCallbackPtr>,
}

impl StreamInner {
    fn play(&mut self) -> Result<(), Error> {
        if !self.playing {
            self.audio_unit
                .start()
                .context("failed to start audio unit")?;
            self.playing = true;
        }
        Ok(())
    }

    fn pause(&mut self) -> Result<(), Error> {
        if self.playing {
            self.audio_unit
                .stop()
                .context("failed to stop audio unit")?;
            self.playing = false;
        }
        Ok(())
    }
}

impl Drop for StreamInner {
    fn drop(&mut self) {
        // SAFETY: This is the sole owning instance of audio_unit (wrapped in
        // ManuallyDrop so we control drop order). Dropping it stops the audio
        // unit, which guarantees CoreAudio will not invoke the render callback
        // after this point. That makes it safe to reclaim the duplex callback
        // pointer below. audio_unit is not accessed after this point.
        unsafe {
            ManuallyDrop::drop(&mut self.audio_unit);
        }

        if let Some(DuplexCallbackPtr(ptr)) = self.duplex_callback_ptr {
            if !ptr.is_null() {
                // SAFETY: ptr created via Box::into_raw, not reclaimed elsewhere.
                unsafe {
                    let _ = Box::from_raw(ptr);
                }
            }
        }
    }
}

pub struct Stream {
    inner: Arc<Mutex<StreamInner>>,
    _disconnect_manager: DisconnectManager,
}

impl Stream {
    fn new(
        inner: StreamInner,
        error_callback: ErrorCallback,
        listen_buffer_size: bool,
    ) -> Result<Self, Error> {
        let device_id = inner.device_id;
        let inner_arc = Arc::new(Mutex::new(inner));
        let weak_inner = Arc::downgrade(&inner_arc);

        let error_callback = Arc::new(Mutex::new(error_callback));
        let disconnect_manager =
            DisconnectManager::new(device_id, weak_inner, error_callback, listen_buffer_size)?;

        Ok(Self {
            inner: inner_arc,
            _disconnect_manager: disconnect_manager,
        })
    }
}

impl StreamTrait for Stream {
    fn play(&self) -> Result<(), Error> {
        self.inner
            .lock()
            .map_err(|_| Error::with_message(ErrorKind::StreamInvalidated, "stream lock poisoned"))?
            .play()
    }

    fn pause(&self) -> Result<(), Error> {
        self.inner
            .lock()
            .map_err(|_| Error::with_message(ErrorKind::StreamInvalidated, "stream lock poisoned"))?
            .pause()
    }

    fn now(&self) -> StreamInstant {
        let m_host_time = unsafe { mach2::mach_time::mach_absolute_time() };
        host_time_to_stream_instant(m_host_time).expect("mach_timebase_info failed")
    }

    fn buffer_size(&self) -> Result<FrameCount, Error> {
        let stream = self.inner.lock().map_err(|_| {
            Error::with_message(ErrorKind::StreamInvalidated, "stream lock poisoned")
        })?;
        device::get_device_buffer_frame_size(&stream.audio_unit)
            .map(|size| size as FrameCount)
            .context("failed to get buffer frame size")
    }
}

#[cfg(test)]
mod test {
    use crate::{
        default_host,
        traits::{DeviceTrait, HostTrait, StreamTrait},
        InputCallbackInfo, OutputCallbackInfo, Sample,
    };

    #[test]
    fn test_play() {
        let host = default_host();
        let device = host.default_output_device().unwrap();

        let mut supported_configs_range = device.supported_output_configs().unwrap();
        let supported_config = supported_configs_range
            .next()
            .unwrap()
            .with_max_sample_rate();
        let config = supported_config.config();

        let stream = device
            .build_output_stream(
                config,
                write_silence::<f32>,
                move |err| println!("Error: {err}"),
                None, // None=blocking, Some(Duration)=timeout
            )
            .unwrap();
        stream.play().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_record() {
        let host = default_host();
        let device = host.default_input_device().unwrap();
        println!("Device: {:?}", device.name());

        let mut supported_configs_range = device.supported_input_configs().unwrap();
        println!("Supported configs:");
        for config in supported_configs_range.clone() {
            println!("{:?}", config)
        }
        let supported_config = supported_configs_range
            .next()
            .unwrap()
            .with_max_sample_rate();
        let config = supported_config.config();

        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &InputCallbackInfo| {
                    // react to stream events and read or write stream data here.
                    println!("Got data: {:?}", &data[..25]);
                },
                move |err| println!("Error: {err}"),
                None, // None=blocking, Some(Duration)=timeout
            )
            .unwrap();
        stream.play().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    #[test]
    fn test_record_output() {
        if std::env::var("CI").is_ok() {
            println!("Skipping test_record_output in CI environment due to permissions");
            return;
        }

        let host = default_host();
        let device = host.default_output_device().unwrap();

        let mut supported_configs_range = device.supported_output_configs().unwrap();
        let supported_config = supported_configs_range
            .next()
            .unwrap()
            .with_max_sample_rate();
        let config = supported_config.config();

        println!("Building input stream");
        let stream = device
            .build_input_stream(
                config,
                move |data: &[f32], _: &InputCallbackInfo| {
                    // react to stream events and read or write stream data here.
                    println!("Got data: {:?}", &data[..25]);
                },
                move |err| println!("Error: {err}"),
                None, // None=blocking, Some(Duration)=timeout
            )
            .unwrap();
        stream.play().unwrap();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }

    fn write_silence<T: Sample>(data: &mut [T], _: &OutputCallbackInfo) {
        for sample in data.iter_mut() {
            *sample = Sample::EQUILIBRIUM;
        }
    }
}
