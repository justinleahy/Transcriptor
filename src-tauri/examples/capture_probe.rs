//! Opens the microphone and system-audio loopback for a few seconds and reports
//! how much audio each delivered. Run: cargo run --example capture_probe [system-source-name]
use std::collections::HashMap;
use std::time::{Duration, Instant};
use transcriptor_lib::audio::{self, CaptureConfig, Source};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let devices = audio::list_devices();
    println!("inputs:  {:?}", devices.inputs);
    println!("outputs: {:?}", devices.outputs);
    println!("monitors: {:?}", devices.monitors);
    println!("default input: {:?}, default output: {:?}", devices.default_input, devices.default_output);

    let (handle, rx, started) = audio::start(CaptureConfig {
        input_device: None,
        capture_system: true,
        system_device: std::env::args().nth(1),
    })?;
    println!("system_active = {}, warnings = {:?}", started.system_active, started.warnings);

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut frames: HashMap<Source, usize> = HashMap::new();
    let mut peak: HashMap<Source, f32> = HashMap::new();
    let mut rates: HashMap<Source, (u32, u16)> = HashMap::new();
    while Instant::now() < deadline {
        if let Ok(chunk) = rx.recv_timeout(Duration::from_millis(100)) {
            *frames.entry(chunk.source).or_default() += chunk.data.len() / chunk.channels as usize;
            let p = chunk.data.iter().fold(0.0f32, |m, s| m.max(s.abs()));
            let e = peak.entry(chunk.source).or_default();
            *e = e.max(p);
            rates.insert(chunk.source, (chunk.sample_rate, chunk.channels));
        }
    }
    handle.stop();
    for src in [Source::Mic, Source::System] {
        let n = frames.get(&src).copied().unwrap_or(0);
        let (rate, ch) = rates.get(&src).copied().unwrap_or((0, 0));
        let secs = if rate > 0 { n as f32 / rate as f32 } else { 0.0 };
        println!(
            "{src:?}: {n} frames ({secs:.2} s) @ {rate} Hz x{ch}, peak {:.4}",
            peak.get(&src).copied().unwrap_or(0.0)
        );
    }
    Ok(())
}
