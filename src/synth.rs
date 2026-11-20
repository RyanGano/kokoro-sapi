//! Kokoro inference pipeline: text → espeak-ng phonemes → token ids → ONNX → f32 PCM.

use std::path::Path;
use std::sync::Once;

use anyhow::{anyhow, Context, Result};
use ort::session::builder::GraphOptimizationLevel;
use ort::session::{RunOptions, Session};
use ort::value::Tensor;

use crate::g2p::Espeak;
use crate::logger::elog;
use crate::tokenizer::Tokenizer;
use crate::voices::VoiceBank;

pub const SAMPLE_RATE: u32 = 24000;

/// Kokoro's context limit: 510 phoneme tokens + 2 padding zeros.
const MAX_TOKENS: usize = 510;

static ORT_INIT: Once = Once::new();

/// Exempt the calling thread from Windows power throttling (EcoQoS).
///
/// On battery, Windows 11 throttles background-ish processes; measured on a
/// 4-core laptop that halves inference speed (RTF ~1.1 vs ~0.55), which turns
/// every chunk into a stall. It only lifts once audio is playing, so the
/// first chunks of a session are hit hardest. This is per thread, so the
/// host process's own threads are untouched.
pub fn exempt_thread_from_power_throttling() {
    use windows::Win32::System::Threading::*;
    thread_local!(static DONE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });
    if DONE.with(|d| d.replace(true)) {
        return;
    }
    let state = THREAD_POWER_THROTTLING_STATE {
        Version: THREAD_POWER_THROTTLING_CURRENT_VERSION,
        ControlMask: THREAD_POWER_THROTTLING_EXECUTION_SPEED,
        StateMask: 0, // controlled, and off
    };
    let r = unsafe {
        SetThreadInformation(
            GetCurrentThread(),
            ThreadPowerThrottling,
            &state as *const _ as *const std::ffi::c_void,
            std::mem::size_of_val(&state) as u32,
        )
    };
    if let Err(e) = r {
        elog!("could not opt thread out of power throttling: {e}");
    }
}

/// Spawns ORT's intra-op worker threads exempt from power throttling.
struct HighQosThreads;

impl ort::environment::ThreadManager for HighQosThreads {
    type Thread = std::thread::JoinHandle<()>;

    fn create(&self, work: impl FnOnce() + Send + 'static) -> ort::Result<Self::Thread> {
        Ok(std::thread::spawn(move || {
            exempt_thread_from_power_throttling();
            work();
        }))
    }

    fn join(thread: Self::Thread) -> ort::Result<()> {
        let _ = thread.join();
        Ok(())
    }
}

pub struct Synth {
    session: Session,
    tokenizer: Tokenizer,
    voices: VoiceBank,
    g2p: Espeak,
    in_ids: String,
    in_style: String,
    in_speed: String,
}

impl Synth {
    pub fn new(assets: &Path) -> Result<Self> {
        ORT_INIT.call_once(|| {
            let dll = assets.join("onnxruntime.dll");
            std::env::set_var("ORT_DYLIB_PATH", &dll);
            // Telemetry off by intent. Note this does NOT prevent the ETW
            // deadlock in ORT < 1.26 (provider registration happens anyway);
            // the real fix is the 1.26+ pin in download-assets.ps1.
            let _ = ort::init().with_telemetry(false).commit();
        });

        let t0 = std::time::Instant::now();
        // Overrides for benchmarking model variants / thread counts.
        let model = std::env::var("KOKORO_SAPI_MODEL")
            .ok()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| assets.join("model.onnx"));
        let mut builder = Session::builder()
            .context("ort session builder")?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(|e| anyhow!("{e}"))?;
        if let Some(n) = std::env::var("KOKORO_SAPI_THREADS").ok().and_then(|s| s.parse().ok()) {
            builder = builder.with_intra_threads(n).map_err(|e| anyhow!("{e}"))?;
        }
        if std::env::var("KOKORO_SAPI_ALLOW_THROTTLING").map_or(true, |v| v.is_empty()) {
            builder = builder.with_thread_manager(HighQosThreads).map_err(|e| anyhow!("{e}"))?;
        }
        let session = builder
            .commit_from_file(&model)
            .with_context(|| format!("loading {}", model.display()))?;
        elog!("model loaded: {} in {} ms", model.display(), t0.elapsed().as_millis());

        // Input names differ between Kokoro ONNX exports (input_ids/tokens).
        let names: Vec<String> = session.inputs().iter().map(|i| i.name().to_string()).collect();
        let pick = |needle: &str, fallback: &str| -> String {
            names
                .iter()
                .find(|n| n.contains(needle))
                .cloned()
                .unwrap_or_else(|| fallback.to_string())
        };
        let in_ids = if names.iter().any(|n| n.contains("id") || n.contains("token")) {
            names
                .iter()
                .find(|n| n.contains("id") || n.contains("token"))
                .unwrap()
                .clone()
        } else {
            "input_ids".to_string()
        };
        let in_style = pick("style", "style");
        let in_speed = pick("speed", "speed");
        elog!("model inputs: {names:?} -> ids={in_ids} style={in_style} speed={in_speed}");

        Ok(Self {
            session,
            tokenizer: Tokenizer::from_file(&assets.join("tokenizer.json"))?,
            voices: VoiceBank::new(assets.join("voices")),
            g2p: Espeak::new(assets)?,
            in_ids,
            in_style,
            in_speed,
        })
    }

    pub fn synthesize(&mut self, text: &str, voice: &str, speed: f32) -> Result<Vec<f32>> {
        self.synthesize_with(text, voice, speed, None)
    }

    /// `run_opts` lets another thread cancel an in-flight inference
    /// (`RunOptions::terminate`) when SAPI aborts.
    pub fn synthesize_with(
        &mut self,
        text: &str,
        voice: &str,
        speed: f32,
        run_opts: Option<&RunOptions>,
    ) -> Result<Vec<f32>> {
        // The calling thread runs part of the graph too.
        if std::env::var("KOKORO_SAPI_ALLOW_THROTTLING").map_or(true, |v| v.is_empty()) {
            exempt_thread_from_power_throttling();
        }
        let t0 = std::time::Instant::now();
        let phonemes = self.g2p.phonemize(text)?;
        let mut ids = self.tokenizer.encode(&phonemes);
        if ids.is_empty() {
            return Err(anyhow!("no tokens for text: {text:?} (phonemes: {phonemes:?})"));
        }
        ids.truncate(MAX_TOKENS);

        let style = self.voices.style(voice, ids.len())?;

        let mut input: Vec<i64> = Vec::with_capacity(ids.len() + 2);
        input.push(0);
        input.extend_from_slice(&ids);
        input.push(0);
        let n = input.len();

        let t_g2p = t0.elapsed();
        let inputs = ort::inputs![
            self.in_ids.as_str() => Tensor::from_array(([1usize, n], input))?,
            self.in_style.as_str() => Tensor::from_array(([1usize, style.len()], style))?,
            self.in_speed.as_str() => Tensor::from_array(([1usize], vec![speed]))?,
        ];
        let outputs = match run_opts {
            Some(o) => self.session.run_with_options(inputs, o)?,
            None => self.session.run(inputs)?,
        };

        let (_, data) = outputs[0].try_extract_tensor::<f32>()?;
        let samples = data.to_vec();
        let total = t0.elapsed();
        let audio_ms = samples.len() as u64 * 1000 / SAMPLE_RATE as u64;
        elog!(
            "synth: {} chars, {} tokens -> {} ms audio in {} ms (g2p {} ms, RTF {:.2})",
            text.chars().count(),
            n - 2,
            audio_ms,
            total.as_millis(),
            t_g2p.as_millis(),
            total.as_millis() as f64 / audio_ms.max(1) as f64
        );
        Ok(samples)
    }
}
