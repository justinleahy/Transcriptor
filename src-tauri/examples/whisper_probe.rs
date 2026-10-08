//! Transcribes a WAV file with the local engine.
//! Run: cargo run --example whisper_probe -- <model.bin> <audio.wav>
use std::time::Instant;
use transcriptor_lib::audio::resample::Resampler;
use transcriptor_lib::transcribe::{local::LocalTranscriber, Transcriber};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let model = std::path::Path::new(&args[1]);
    let mut reader = hound::WavReader::open(&args[2])?;
    let spec = reader.spec();
    let raw: Vec<f32> = match spec.sample_format {
        hound::SampleFormat::Int => {
            let max = (1u64 << (spec.bits_per_sample - 1)) as f32;
            reader.samples::<i32>().map(|s| s.unwrap() as f32 / max).collect()
        }
        hound::SampleFormat::Float => reader.samples::<f32>().map(|s| s.unwrap()).collect(),
    };
    let mut rs = Resampler::new(spec.sample_rate);
    let mono = rs.process(&raw, spec.channels);
    println!("{} samples @16k ({:.1} s)", mono.len(), mono.len() as f32 / 16000.0);

    let t = Instant::now();
    let mut engine = LocalTranscriber::new(model)?;
    println!("model loaded in {} ms", t.elapsed().as_millis());
    let t = Instant::now();
    let utts = engine.transcribe(&mono, "en")?;
    println!("transcribed in {} ms", t.elapsed().as_millis());
    for u in utts {
        println!("[{:>6} - {:>6}] {}", u.start_ms, u.end_ms, u.text);
    }
    drop(engine);
    transcriptor_lib::transcribe::local::clear_cache();
    println!("engine dropped cleanly");
    Ok(())
}
