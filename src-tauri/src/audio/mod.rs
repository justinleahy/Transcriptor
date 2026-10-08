//! Audio capture via cpal. Two sources are captured: the microphone ("you")
//! and system output / loopback ("others"). Raw interleaved samples are sent to
//! a processing thread. Each cpal stream is owned by its own thread because
//! `cpal::Stream` is not `Send` on every platform, and because opening a stream
//! can block on an OS permission prompt — a per-stream thread lets us time out
//! the optional loopback stream instead of freezing the whole app.
//!
//! "Others" comes from one of two places:
//! - an explicitly chosen *input* device (`CaptureConfig::system_device`): a
//!   PulseAudio/PipeWire monitor source on Linux, or a virtual device such as
//!   BlackHole (macOS) or "Stereo Mix" (Windows);
//! - automatic loopback: an input stream on the default *output* device (Core
//!   Audio process tap on macOS 14.6+, WASAPI loopback on Windows). On Linux
//!   the automatic mode first looks for the monitor source of the default
//!   output, because output devices there cannot be recorded directly.

pub mod resample;

use crate::models::AudioDevices;
use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, Sample, SampleFormat, SupportedStreamConfig};
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Mic,
    System,
}

/// A block of raw interleaved samples straight from the device callback.
pub struct RawChunk {
    pub source: Source,
    /// When the device callback delivered this block (≈ its last sample).
    pub captured_at: Instant,
    pub channels: u16,
    pub sample_rate: u32,
    pub data: Vec<f32>,
}

pub struct CaptureConfig {
    pub input_device: Option<String>,
    pub capture_system: bool,
    /// Input device to record as "Others" instead of automatic loopback.
    pub system_device: Option<String>,
}

pub struct CaptureStarted {
    pub system_active: bool,
    pub warnings: Vec<String>,
}

/// How long to wait for the microphone to open. Generous because the first
/// open blocks on the OS microphone permission prompt.
const MIC_OPEN_TIMEOUT: Duration = Duration::from_secs(90);
/// How long to wait for system-audio loopback before falling back to mic-only.
const LOOPBACK_OPEN_TIMEOUT: Duration = Duration::from_secs(45);

/// Owns one stream thread; dropping (or `stop`) closes the stream.
struct StreamThread {
    stop_tx: Option<Sender<()>>,
}

impl Drop for StreamThread {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
    }
}

pub struct CaptureHandle {
    streams: Vec<StreamThread>,
}

impl CaptureHandle {
    pub fn stop(mut self) {
        self.streams.clear();
    }
}

fn device_name(d: &Device) -> String {
    d.description()
        .map(|desc| desc.name().to_string())
        .unwrap_or_else(|_| "Unknown device".to_string())
}

/// The cpal host to use. On Linux, prefer the PulseAudio host (which also talks
/// to PipeWire through pipewire-pulse) because it exposes monitor sources for
/// system audio; ALSA does not. Everywhere else, the platform default.
fn audio_host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    {
        if cpal::available_hosts().contains(&cpal::HostId::PulseAudio) {
            match cpal::host_from_id(cpal::HostId::PulseAudio) {
                Ok(host) => return host,
                Err(e) => log::warn!("PulseAudio host unavailable ({e}); using the default audio host"),
            }
        }
    }
    cpal::default_host()
}

/// Whether an input device name looks like a monitor / loopback source that
/// carries system audio rather than a microphone.
pub fn is_monitor_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    lower.contains("monitor")
        || lower.ends_with(".monitor")
        || lower.contains("blackhole")
        || lower.contains("stereo mix")
        || lower.contains("loopback")
        || lower.contains("what u hear")
}

/// Pick the input that best carries the default output's audio: the monitor
/// of the default output ("Monitor of <output>" or "<output>.monitor"), else
/// any monitor whose name mentions the output, else any monitor-like input.
/// Returns an index into `inputs`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pick_monitor(inputs: &[String], default_output: Option<&str>) -> Option<usize> {
    if let Some(out) = default_output.map(str::to_lowercase).filter(|o| !o.is_empty()) {
        let exact = [format!("monitor of {out}"), format!("{out}.monitor")];
        if let Some(i) = inputs.iter().position(|n| exact.contains(&n.to_lowercase())) {
            return Some(i);
        }
        if let Some(i) = inputs
            .iter()
            .position(|n| is_monitor_name(n) && n.to_lowercase().contains(&out))
        {
            return Some(i);
        }
    }
    inputs.iter().position(|n| is_monitor_name(n))
}

pub fn list_devices() -> AudioDevices {
    let host = audio_host();
    let inputs: Vec<String> = host
        .input_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    let monitors = inputs.iter().filter(|n| is_monitor_name(n)).cloned().collect();
    let outputs = host
        .output_devices()
        .map(|it| it.map(|d| device_name(&d)).collect())
        .unwrap_or_default();
    AudioDevices {
        inputs,
        outputs,
        monitors,
        default_input: host.default_input_device().map(|d| device_name(&d)),
        default_output: host.default_output_device().map(|d| device_name(&d)),
    }
}

fn find_input_device(host: &cpal::Host, name: Option<&str>) -> Result<Device> {
    if let Some(name) = name {
        if let Ok(mut devices) = host.input_devices() {
            if let Some(d) = devices.find(|d| device_name(d) == name) {
                return Ok(d);
            }
        }
        log::warn!("input device '{name}' not found; falling back to default");
    }
    host.default_input_device()
        .ok_or_else(|| anyhow!("no microphone / input device available"))
}

fn build_stream(
    device: &Device,
    config: &SupportedStreamConfig,
    source: Source,
    tx: Sender<RawChunk>,
) -> Result<cpal::Stream> {
    let channels = config.channels();
    let sample_rate = config.sample_rate();
    let stream_config = config.config();
    let err_fn = move |e| log::error!("{source:?} stream error: {e}");
    let timeout = Some(Duration::from_secs(2));

    macro_rules! input_stream {
        ($t:ty, $conv:expr) => {{
            let tx = tx.clone();
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[$t], _| {
                    let conv: fn($t) -> f32 = $conv;
                    let chunk = RawChunk {
                        source,
                        captured_at: Instant::now(),
                        channels,
                        sample_rate,
                        data: data.iter().map(|s| conv(*s)).collect(),
                    };
                    // Never block the realtime callback; drop if the consumer is behind.
                    let _ = tx.try_send(chunk);
                },
                err_fn,
                timeout,
            )?
        }};
    }

    let stream = match config.sample_format() {
        SampleFormat::F32 => input_stream!(f32, |s| s),
        SampleFormat::I16 => input_stream!(i16, |s| s as f32 / 32768.0),
        SampleFormat::U16 => input_stream!(u16, |s| (s as f32 - 32768.0) / 32768.0),
        SampleFormat::I32 => input_stream!(i32, |s| s as f32 / 2147483648.0),
        // PulseAudio reports 24-bit sources as I24 (in a 32-bit container).
        SampleFormat::I24 => input_stream!(cpal::I24, |s| s.to_sample::<f32>()),
        other => return Err(anyhow!("unsupported sample format {other:?}")),
    };
    Ok(stream)
}

/// Open `device` as an input stream feeding `source` and start it.
fn open_input(device: &Device, source: Source, what: &str, tx: Sender<RawChunk>) -> Result<cpal::Stream> {
    let config = device
        .default_input_config()
        .with_context(|| format!("querying {what} format"))?;
    log::info!(
        "{what}: {} @ {} Hz x{} {:?}",
        device_name(device),
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    );
    let stream = build_stream(device, &config, source, tx)?;
    stream.play().with_context(|| format!("starting {what} stream"))?;
    Ok(stream)
}

fn open_mic(host: &cpal::Host, name: Option<&str>, tx: Sender<RawChunk>) -> Result<cpal::Stream> {
    let device = find_input_device(host, name)?;
    open_input(&device, Source::Mic, "microphone", tx)
}

/// System audio from an explicitly chosen input device (monitor source,
/// BlackHole, Stereo Mix, ...). Never falls back to the default input: that
/// would record the microphone as "Others".
fn open_system_device(host: &cpal::Host, name: &str, tx: Sender<RawChunk>) -> Result<cpal::Stream> {
    let device = host
        .input_devices()
        .context("listing input devices")?
        .find(|d| device_name(d) == name)
        .ok_or_else(|| anyhow!("system audio source \"{name}\" was not found; pick another source in Settings"))?;
    open_input(&device, Source::System, "system audio source", tx)
}

/// On Linux, the monitor input that carries the default output's audio.
#[cfg(target_os = "linux")]
fn find_monitor_input(host: &cpal::Host) -> Option<Device> {
    let inputs: Vec<Device> = host.input_devices().ok()?.collect();
    let names: Vec<String> = inputs.iter().map(device_name).collect();
    let default_output = host.default_output_device().map(|d| device_name(&d));
    let i = pick_monitor(&names, default_output.as_deref())?;
    inputs.into_iter().nth(i)
}

/// Automatic system-audio capture on Linux. Output devices cannot be recorded
/// directly there, so use the default output's PulseAudio/PipeWire monitor
/// source (or any monitor-like input), then try cpal's output loopback as a
/// last resort.
#[cfg(target_os = "linux")]
fn open_loopback(host: &cpal::Host, tx: Sender<RawChunk>) -> Result<cpal::Stream> {
    if let Some(device) = find_monitor_input(host) {
        match open_input(&device, Source::System, "system audio monitor", tx.clone()) {
            Ok(stream) => return Ok(stream),
            Err(e) => log::warn!("monitor source {} failed: {e:#}", device_name(&device)),
        }
    }
    open_output_loopback(host, tx).map_err(|e| {
        log::warn!("output loopback failed: {e:#}");
        anyhow!(
            "no system-audio monitor source was found. Choose a monitor source under \
             Settings > Audio > Other participants (system audio) source"
        )
    })
}

/// Automatic system-audio capture on macOS (Core Audio process tap, 14.6+)
/// and Windows (WASAPI loopback): cpal records an *output* device when an
/// input stream is built on it.
#[cfg(not(target_os = "linux"))]
fn open_loopback(host: &cpal::Host, tx: Sender<RawChunk>) -> Result<cpal::Stream> {
    open_output_loopback(host, tx)
}

/// Record the default output device via cpal loopback.
fn open_output_loopback(host: &cpal::Host, tx: Sender<RawChunk>) -> Result<cpal::Stream> {
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow!("no output device available"))?;
    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(_) => device
            .default_output_config()
            .context("querying output device format")?,
    };
    log::info!(
        "loopback: {} @ {} Hz x{} {:?}",
        device_name(&device),
        config.sample_rate(),
        config.channels(),
        config.sample_format()
    );
    let stream = build_stream(&device, &config, Source::System, tx)?;
    stream.play().context("starting system-audio stream")?;
    log::info!("loopback stream running");
    Ok(stream)
}

/// Spawn a thread that opens one stream and keeps it alive until stopped.
/// Returns the readiness receiver (Ok once playing) and the thread handle.
fn spawn_stream_thread<F>(name: &str, open: F) -> (Receiver<Result<()>>, StreamThread)
where
    F: FnOnce(&cpal::Host) -> Result<cpal::Stream> + Send + 'static,
{
    let (ready_tx, ready_rx) = bounded::<Result<()>>(1);
    let (stop_tx, stop_rx) = bounded::<()>(1);
    let thread_name = name.to_string();
    let spawn = std::thread::Builder::new()
        .name(format!("audio-{name}"))
        .spawn(move || {
            let host = audio_host();
            let stream = match open(&host) {
                Ok(s) => s,
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let _ = ready_tx.send(Ok(()));
            // Block until asked to stop (or the handle is dropped); the stream
            // is dropped — and closed — when this thread exits.
            let _ = stop_rx.recv();
            drop(stream);
            log::debug!("{thread_name} stream closed");
        });
    if let Err(e) = spawn {
        let (tx, rx) = bounded::<Result<()>>(1);
        let _ = tx.send(Err(anyhow!("spawning {name} thread: {e}")));
        return (rx, StreamThread { stop_tx: None });
    }
    (
        ready_rx,
        StreamThread {
            stop_tx: Some(stop_tx),
        },
    )
}

fn wait_ready(rx: &Receiver<Result<()>>, timeout: Duration, what: &str) -> Result<()> {
    match rx.recv_timeout(timeout) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => Err(anyhow!(
            "{what} did not start within {} s (waiting on a permission prompt?)",
            timeout.as_secs()
        )),
        Err(RecvTimeoutError::Disconnected) => Err(anyhow!("{what} thread exited unexpectedly")),
    }
}

/// Start capturing. Returns once the microphone is running; loopback is
/// optional and reported via `CaptureStarted::system_active`. Raw chunks arrive
/// on the returned receiver until the handle is stopped or dropped.
pub fn start(cfg: CaptureConfig) -> Result<(CaptureHandle, Receiver<RawChunk>, CaptureStarted)> {
    let (data_tx, data_rx) = bounded::<RawChunk>(512);
    let mut warnings = Vec::new();
    let mut streams = Vec::new();

    let mic_name = cfg.input_device.clone();
    let mic_tx = data_tx.clone();
    let (mic_ready, mic_thread) =
        spawn_stream_thread("mic", move |host| open_mic(host, mic_name.as_deref(), mic_tx));
    streams.push(mic_thread);
    wait_ready(&mic_ready, MIC_OPEN_TIMEOUT, "microphone")?;

    let system_active = if cfg.capture_system {
        let sys_tx = data_tx.clone();
        let sys_device = cfg.system_device.clone();
        let (sys_ready, sys_thread) = spawn_stream_thread("loopback", move |host| match sys_device {
            Some(name) => open_system_device(host, &name, sys_tx),
            None => open_loopback(host, sys_tx),
        });
        match wait_ready(&sys_ready, LOOPBACK_OPEN_TIMEOUT, "system audio") {
            Ok(()) => {
                streams.push(sys_thread);
                true
            }
            Err(e) => {
                log::warn!("system audio unavailable: {e:#}");
                warnings.push(format!(
                    "System audio capture is unavailable ({e}). Only your microphone is being recorded."
                ));
                // Dropping the thread handle asks it to stop if it ever finishes opening.
                drop(sys_thread);
                false
            }
        }
    } else {
        false
    };

    drop(data_tx);
    Ok((
        CaptureHandle { streams },
        data_rx,
        CaptureStarted {
            system_active,
            warnings,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_heuristic() {
        for name in [
            "Monitor of Built-in Audio Analog Stereo",
            "alsa_output.pci-0000_00_1f.3.analog-stereo.monitor",
            "BlackHole 2ch",
            "Stereo Mix (Realtek(R) Audio)",
            "Loopback Audio",
            "What U Hear (Sound Blaster)",
        ] {
            assert!(is_monitor_name(name), "{name}");
        }
        for name in ["MacBook Pro Microphone", "USB Audio Device", "Headset (Jabra)", "Built-in Audio Analog Stereo"] {
            assert!(!is_monitor_name(name), "{name}");
        }
    }

    #[test]
    fn picks_monitor_of_default_output() {
        let inputs: Vec<String> = [
            "Built-in Audio Analog Stereo",
            "Monitor of HDMI Audio",
            "Monitor of Built-in Audio Analog Stereo",
            "Webcam Mic",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(pick_monitor(&inputs, Some("Built-in Audio Analog Stereo")), Some(2));
        assert_eq!(pick_monitor(&inputs, Some("built-in audio analog stereo")), Some(2));
        // Unknown output: first monitor-like input.
        assert_eq!(pick_monitor(&inputs, Some("Bluetooth Headphones")), Some(1));
        assert_eq!(pick_monitor(&inputs, None), Some(1));

        let raw: Vec<String> = ["alsa_input.mic", "alsa_output.usb.monitor", "alsa_output.pci.monitor"]
            .map(String::from)
            .to_vec();
        assert_eq!(pick_monitor(&raw, Some("alsa_output.pci")), Some(2));

        let mics: Vec<String> = ["Webcam Mic", "Headset"].map(String::from).to_vec();
        assert_eq!(pick_monitor(&mics, Some("Speakers")), None);
    }
}
