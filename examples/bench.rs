//! Synthesis timing outside SAPI: model load, warm-up, and per-sentence
//! inference vs. audio length (real-time factor).
//!
//!   cargo run --release --example bench [VOICE] [SENTENCE_COUNT]
//!
//! KOKORO_SAPI_MODEL / KOKORO_SAPI_THREADS override the model file and ORT
//! intra-op thread count; KOKORO_SAPI_ALLOW_THROTTLING=1 disables the
//! power-throttling exemption (to measure its effect).

use std::time::Instant;

use kokoro_sapi::synth::{Synth, SAMPLE_RATE};

const SENTENCES: &[&str] = &[
    "In the beginning God created the heavens and the earth.",
    "Now the earth was formless and empty, darkness was over the surface of the deep, and the Spirit of God was hovering over the waters.",
    "And God said, Let there be light, and there was light.",
    "God saw that the light was good, and he separated the light from the darkness.",
    "Paul, an apostle of Christ Jesus by the will of God, to the saints who are in Ephesus, and are faithful in Christ Jesus: Grace to you and peace from God our Father and the Lord Jesus Christ.",
    "Yes.",
];

fn main() -> anyhow::Result<()> {
    let voice = std::env::args().nth(1).unwrap_or_else(|| "af_heart".into());
    let count: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(SENTENCES.len());
    let assets = kokoro_sapi::config::assets_dir(None);

    let t = Instant::now();
    let mut synth = Synth::new(&assets)?;
    println!("load: {:.0} ms", t.elapsed().as_secs_f64() * 1000.0);

    for round in 0..2 {
        println!("-- round {round}");
        for s in &SENTENCES[..count.min(SENTENCES.len())] {
            let t = Instant::now();
            let pcm = synth.synthesize(s, &voice, 1.0)?;
            let ms = t.elapsed().as_secs_f64() * 1000.0;
            let audio_ms = pcm.len() as f64 * 1000.0 / SAMPLE_RATE as f64;
            let ms_of = |n: usize| n as f64 * 1000.0 / SAMPLE_RATE as f64;
            let lead = pcm.iter().position(|v| v.abs() > 0.01).unwrap_or(pcm.len());
            let tail = pcm.iter().rev().position(|v| v.abs() > 0.01).unwrap_or(pcm.len());
            print!("[silence: lead {:.0} ms, tail {:.0} ms] ", ms_of(lead), ms_of(tail));
            println!(
                "{ms:6.0} ms for {audio_ms:6.0} ms audio (RTF {:.2})  {} chars",
                ms / audio_ms,
                s.len()
            );
        }
    }
    Ok(())
}
