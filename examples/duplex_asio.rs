//! Full-duplex passthrough using the ASIO backend.
//!
//! Captures the input (e.g. microphone / line-in) and renders it straight back out through a
//! single synchronized duplex stream, where input and output share one clock and fire from one
//! device callback. This avoids the input/output drift that the separate-stream `feedback`
//! example compensates for with a delay ring buffer.
//!
//! Requires the `asio` feature and an ASIO driver (real hardware or ASIO4ALL). See the README
//! section "Compiling for ASIO" for build setup (`LIBCLANG_PATH`, the ASIO SDK, and MSVC).
//!
//! Run with: `cargo run --example duplex_asio --features asio`
//! With a specific device: `cargo run --example duplex_asio --features asio -- --device "<id>"`
//!
//! WARNING: feeding a live microphone straight to speakers can cause loud feedback. Use
//! headphones, or keep the volume low.

use clap::Parser;
use cpal::{
    traits::{DeviceTrait, HostTrait, StreamTrait},
    Device, DuplexCallbackInfo, DuplexStreamConfig, Error, ErrorKind, HostId, SampleFormat,
    SizedSample,
};

#[derive(Parser, Debug)]
#[command(version, about = "CPAL ASIO duplex passthrough example", long_about = None)]
struct Opt {
    /// The ASIO device to use. Defaults to the default input device.
    #[arg(short, long)]
    device: Option<String>,

    /// How many seconds to run before stopping. Omit to run indefinitely (Ctrl+C to stop).
    #[arg(short, long)]
    seconds: Option<u64>,
}

fn main() -> anyhow::Result<()> {
    let opt = Opt::parse();

    let host = cpal::host_from_id(HostId::Asio)
        .expect("ASIO host unavailable; build with `--features asio` on Windows");

    // A duplex stream drives both directions from one device, so we select a single device that
    // exposes both capture and playback. Match the `--device` flag against the ASIO driver name
    // (e.g. "ASIO4ALL v2"), which is the backend-specific portion of the device id.
    let device = match opt.device {
        Some(name) => host
            .devices()?
            .find(|d| d.id().is_ok_and(|id| id.id() == name))
            .ok_or_else(|| {
                let available: Vec<String> = host
                    .devices()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| d.id().ok().map(|id| id.id().to_string()))
                    .collect();
                anyhow::anyhow!("no ASIO device named \"{name}\"; available: {available:?}")
            })?,
        None => host
            .default_input_device()
            .expect("failed to find an ASIO device"),
    };

    println!("Using ASIO device: \"{}\"", device.id()?);

    if !device.supports_duplex() {
        anyhow::bail!("device does not report synchronized duplex support");
    }

    // ASIO uses one native sample type per device, so the input and output default configs report
    // the same format. We drive the stream in that native format to avoid any conversion.
    let input_config = device.default_input_config()?;
    let output_config = device.default_output_config()?;
    println!("Default input config:  {input_config:?}");
    println!("Default output config: {output_config:?}");

    if input_config.sample_format() != output_config.sample_format() {
        anyhow::bail!(
            "input ({}) and output ({}) sample formats differ; duplex needs a single format",
            input_config.sample_format(),
            output_config.sample_format()
        );
    }
    if input_config.sample_rate() != output_config.sample_rate() {
        anyhow::bail!("input and output sample rates differ");
    }

    let config = DuplexStreamConfig {
        input_channels: input_config.channels(),
        output_channels: output_config.channels(),
        sample_rate: input_config.sample_rate(),
        // Let the driver pick its configured buffer size (set in the ASIO control panel).
        buffer_size: cpal::BufferSize::Default,
    };
    println!("Duplex config: {config:?}");

    match input_config.sample_format() {
        SampleFormat::I16 => run::<i16>(&device, config, opt.seconds),
        SampleFormat::I32 => run::<i32>(&device, config, opt.seconds),
        SampleFormat::F32 => run::<f32>(&device, config, opt.seconds),
        SampleFormat::F64 => run::<f64>(&device, config, opt.seconds),
        // ASIO commonly reports one of the above; extend as needed for exotic drivers.
        sample_format => anyhow::bail!("unsupported ASIO sample format '{sample_format}'"),
    }
}

fn run<T>(device: &Device, config: DuplexStreamConfig, seconds: Option<u64>) -> anyhow::Result<()>
where
    T: SizedSample + Send + 'static,
{
    let input_channels = config.input_channels as usize;
    let output_channels = config.output_channels as usize;
    // Copy as many channels as both sides share; pad any extra output channels with silence.
    let shared_channels = input_channels.min(output_channels);

    let data_fn = move |input: &[T], output: &mut [T], _: &DuplexCallbackInfo| {
        let in_frames = input.chunks_exact(input_channels);
        let out_frames = output.chunks_exact_mut(output_channels);
        for (in_frame, out_frame) in in_frames.zip(out_frames) {
            out_frame[..shared_channels].copy_from_slice(&in_frame[..shared_channels]);
            for sample in &mut out_frame[shared_channels..] {
                *sample = T::EQUILIBRIUM;
            }
        }
    };

    println!("Building duplex stream...");
    let stream = device.build_duplex_stream(config, data_fn, err_fn, None)?;
    println!("Successfully built duplex stream.");

    stream.play()?;
    match seconds {
        Some(seconds) => {
            println!("Passing input through to output for {seconds} seconds (mind the feedback!)...");
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            drop(stream);
            println!("Done!");
        }
        None => {
            println!("Passing input through to output indefinitely (mind the feedback!).");
            println!("Press Ctrl+C to stop.");
            // Catch Ctrl+C so we can break the loop and drop the stream cleanly, rather than
            // relying on the OS default handler to kill the process mid-callback.
            let running = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let r = running.clone();
            ctrlc::set_handler(move || r.store(false, std::sync::atomic::Ordering::SeqCst))?;
            while running.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            drop(stream);
            println!("\nStopped.");
        }
    }
    Ok(())
}

fn err_fn(err: Error) {
    match err.kind() {
        ErrorKind::DeviceChanged | ErrorKind::Xrun | ErrorKind::RealtimeDenied => {
            eprintln!("{err}")
        }
        _ => eprintln!("Stream error: {err}"),
    }
}
