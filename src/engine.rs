//! The SAPI5 TTS engine object: ISpTTSEngine + ISpObjectWithToken.
//!
//! SAPI calls `SetObjectToken` right after creation (the token is the registry
//! voice entry, carrying our config), then `GetOutputFormat` and `Speak`.
//!
//! Speak runs a two-stage pipeline: a worker thread synthesizes chunk N+1
//! while this (SAPI) thread paces chunk N out to the audio device. Synthesis
//! runs at roughly real time on a laptop CPU, so without the overlap every
//! chunk boundary is an audible pause.

use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::Media::Audio::{WAVEFORMATEX, WAVE_FORMAT_PCM};
use windows::Win32::Media::Speech::*;
use windows::Win32::System::Com::{CoTaskMemAlloc, CoTaskMemFree};

use crate::logger::elog;
use crate::synth::{Synth, SAMPLE_RATE};

/// SPDFID_WaveFormatEx from sapi.h (not exposed by the windows crate).
const SPDFID_WAVEFORMATEX: GUID = GUID::from_u128(0xc31adbae_527f_4ff5_a230_f62bb61ff70c);

const BYTES_PER_MS: u32 = SAMPLE_RATE * 2 / 1000;

// SPVESACTIONS flags returned by ISpTTSEngineSite::GetActions.
const SPVES_ABORT: u32 = 1;
#[allow(dead_code)]
const SPVES_SKIP: u32 = 2;
const SPVES_RATE: u32 = 4;
const SPVES_VOLUME: u32 = 8;

struct EngineState {
    token: Option<ISpObjectToken>,
    voice: String,
    assets: PathBuf,
    tail_lead: Duration,
    /// When the previous Speak returned, and how much of its audio was still
    /// queued then (None after an abort). Shows how the host feeds text (one
    /// call per paragraph vs. per sentence) and whether the next call's first
    /// audio arrives before that queue runs dry.
    last_speak_end: Option<(Instant, Option<Duration>)>,
}

// Safety: SAPI may call the engine from different threads (ThreadingModel=Both),
// but serializes calls per voice instance; the Mutex makes this explicit.
unsafe impl Send for EngineState {}

#[implement(ISpTTSEngine, ISpObjectWithToken)]
pub struct KokoroEngine {
    state: Mutex<EngineState>,
}

impl KokoroEngine {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(EngineState {
                token: None,
                voice: "af_heart".into(),
                assets: crate::config::assets_dir(None),
                tail_lead: Duration::from_millis(DEFAULT_TAIL_LEAD_MS as u64),
                last_speak_end: None,
            }),
        }
    }
}

fn token_string(token: &ISpObjectToken, name: &str) -> Option<String> {
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let pwstr = token.GetStringValue(PCWSTR(wide.as_ptr())).ok()?;
        if pwstr.is_null() {
            return None;
        }
        let s = pwstr.to_string().ok();
        CoTaskMemFree(Some(pwstr.as_ptr() as *const c_void));
        s
    }
}

impl ISpObjectWithToken_Impl for KokoroEngine_Impl {
    fn SetObjectToken(&self, ptoken: Ref<ISpObjectToken>) -> Result<()> {
        let token = ptoken.ok()?.clone();
        let assets = crate::config::assets_dir(token_string(&token, "AssetsDir"));
        crate::logger::set_dir(&assets);
        let mut state = self.state.lock().unwrap();
        if let Some(v) = token_string(&token, "VoiceName") {
            state.voice = v;
        }
        if let Some(ms) = token_string(&token, "TailLeadMs").and_then(|s| s.trim().parse::<u64>().ok()) {
            state.tail_lead = Duration::from_millis(ms);
        }
        elog!(
            "SetObjectToken: voice={} assets={} tail_lead={} ms",
            state.voice,
            assets.display(),
            state.tail_lead.as_millis()
        );
        state.token = Some(token);
        state.assets = assets.clone();
        drop(state);
        // Load and warm up the model in the background now, so the first
        // Speak doesn't pay for it. Hosts create a new engine per voice
        // object; the model is shared process-wide, so after the first time
        // this returns immediately.
        std::thread::spawn(move || {
            let _ = shared_synth(&assets);
        });
        Ok(())
    }

    fn GetObjectToken(&self) -> Result<ISpObjectToken> {
        self.state
            .lock()
            .unwrap()
            .token
            .clone()
            .ok_or_else(|| Error::from(E_FAIL))
    }
}

impl ISpTTSEngine_Impl for KokoroEngine_Impl {
    fn Speak(
        &self,
        _dwspeakflags: u32,
        _rguidformatid: *const GUID,
        _pwaveformatex: *const WAVEFORMATEX,
        ptextfraglist: *const SPVTEXTFRAG,
        poutputsite: Ref<ISpTTSEngineSite>,
    ) -> Result<()> {
        let site = poutputsite.ok()?.clone();
        let result = catch_unwind(AssertUnwindSafe(|| self.speak_inner(ptextfraglist, &site)));
        match result {
            Ok(r) => r,
            Err(_) => {
                elog!("PANIC in Speak — returning E_FAIL");
                Err(E_FAIL.into())
            }
        }
    }

    fn GetOutputFormat(
        &self,
        _ptargetfmtid: *const GUID,
        _ptargetwaveformatex: *const WAVEFORMATEX,
        poutputformatid: *mut GUID,
        ppcomemoutputwaveformatex: *mut *mut WAVEFORMATEX,
    ) -> Result<()> {
        unsafe {
            if poutputformatid.is_null() || ppcomemoutputwaveformatex.is_null() {
                return Err(E_POINTER.into());
            }
            let wfx = CoTaskMemAlloc(std::mem::size_of::<WAVEFORMATEX>()) as *mut WAVEFORMATEX;
            if wfx.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            *wfx = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 1,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * 2,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            *poutputformatid = SPDFID_WAVEFORMATEX;
            *ppcomemoutputwaveformatex = wfx;
        }
        Ok(())
    }
}

/// One text run extracted from the fragment list, with its source offset
/// (UTF-16 units) for word-boundary events.
enum Frag {
    Text { utf16: Vec<u16>, src_offset: u32 },
    SilenceMs(u32),
}

/// One unit of output, in playback order.
enum Item {
    Silence(u32),
    Speech { sentence: Sentence, src_offset: u32 },
}

type Chunk = anyhow::Result<Vec<f32>>;

/// State shared between the SAPI thread and the synthesis worker.
struct Shared {
    cancel: AtomicBool,
    rate_adj: AtomicI32,
    /// Lets the SAPI thread abort an in-flight inference on stop/purge.
    run_opts: Option<ort::session::RunOptions>,
}

impl Shared {
    fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(o) = &self.run_opts {
            let _ = o.terminate();
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

/// SAPI rate −10..10 → Kokoro speed 0.5×..2×.
fn speed_for(rate_adj: i32) -> f32 {
    2f32.powf(rate_adj as f32 / 10.0).clamp(0.5, 2.0)
}

impl KokoroEngine_Impl {
    fn speak_inner(&self, ptextfraglist: *const SPVTEXTFRAG, site: &ISpTTSEngineSite) -> Result<()> {
        let t_start = Instant::now();
        let frags = unsafe { collect_frags(ptextfraglist) };
        let rate_adj = unsafe { site.GetRate().unwrap_or(0) };
        let volume = unsafe { site.GetVolume().unwrap_or(100) };

        let (voice, assets, tail_lead, gap) = {
            let state = self.state.lock().unwrap();
            (
                state.voice.clone(),
                state.assets.clone(),
                state.tail_lead,
                state.last_speak_end,
            )
        };

        let mut items = Vec::new();
        let mut chars = 0usize;
        let mut n_chunks = 0usize;
        for frag in frags {
            match frag {
                Frag::SilenceMs(ms) => items.push(Item::Silence(ms)),
                Frag::Text { utf16, src_offset } => {
                    let text = String::from_utf16_lossy(&utf16);
                    chars += text.chars().count();
                    // Only the first chunk of the call gets the short clause
                    // limit: it's the one the listener is waiting on.
                    for sentence in split_sentences(&text, n_chunks == 0) {
                        n_chunks += 1;
                        items.push(Item::Speech { sentence, src_offset });
                    }
                }
            }
        }
        elog!(
            "Speak: {chars} chars, {n_chunks} chunks, rate={rate_adj}, gap since last Speak: {}",
            gap.map(|(t, _)| format!("{} ms", t.elapsed().as_millis()))
                .unwrap_or_else(|| "n/a".into())
        );

        let shared = Arc::new(Shared {
            cancel: AtomicBool::new(false),
            rate_adj: AtomicI32::new(rate_adj),
            run_opts: ort::session::RunOptions::new().ok(),
        });
        let texts: Vec<String> = items
            .iter()
            .filter_map(|i| match i {
                Item::Speech { sentence, .. } => Some(sentence.text.clone()),
                Item::Silence(_) => None,
            })
            .collect();
        let rx = spawn_producer(texts, voice, assets, shared.clone());

        let mut queued_at_end = None;
        let prev_end = gap.and_then(|(t, q)| q.map(|q| (t, q)));
        let result = play(site, items, &rx, &shared, volume, tail_lead, t_start, prev_end, &mut queued_at_end);
        // Stop the worker (if still running) without waiting for it: a new
        // Speak after a purge should not sit behind the old one's inference.
        shared.cancel();
        drop(rx);
        self.state.lock().unwrap().last_speak_end = Some((Instant::now(), queued_at_end));
        elog!(
            "Speak done after {} ms ({} ms of audio still queued)",
            t_start.elapsed().as_millis(),
            queued_at_end.map(|d| d.as_millis()).unwrap_or(0)
        );
        result
    }
}

/// The consumer side: receive synthesized chunks in order and write them to
/// SAPI, paced to real time. `prev_end` = when the previous Speak returned
/// and how much audio it left queued, to detect a gap between calls.
#[allow(clippy::too_many_arguments)]
fn play(
    site: &ISpTTSEngineSite,
    items: Vec<Item>,
    rx: &Receiver<Chunk>,
    shared: &Shared,
    mut volume: u16,
    tail_lead: Duration,
    t_start: Instant,
    prev_end: Option<(Instant, Duration)>,
    queued_at_end: &mut Option<Duration>,
) -> Result<()> {
    let mut bytes_written: u64 = 0;
    let mut pacer = Pacer::new();
    let mut first_audio = true;
    let last_speech = items.iter().rposition(|i| matches!(i, Item::Speech { .. }));

    for (idx, item) in items.into_iter().enumerate() {
        if Some(idx) == last_speech {
            pacer.max_lead = tail_lead.max(MAX_LEAD);
        }
        match item {
            Item::Silence(ms) => {
                let buf = vec![0u8; (ms * BYTES_PER_MS) as usize & !1];
                if !write_pcm(site, &buf, &mut bytes_written, &mut pacer) {
                    return Ok(());
                }
            }
            Item::Speech { sentence, src_offset } => {
                let actions = unsafe { site.GetActions() };
                if actions & SPVES_ABORT != 0 {
                    elog!("abort requested");
                    return Ok(());
                }
                if actions & SPVES_RATE != 0 {
                    // Applies from the next chunk the worker starts.
                    let r = unsafe { site.GetRate().unwrap_or(0) };
                    shared.rate_adj.store(r, Ordering::SeqCst);
                }
                if actions & SPVES_VOLUME != 0 {
                    volume = unsafe { site.GetVolume().unwrap_or(volume) };
                }

                // Audio still queued ahead of playback when we start waiting:
                // if the chunk takes longer than that, the listener hears a gap.
                let buffered = pacer.lead(bytes_written);
                let wait_start = Instant::now();
                let samples = match recv_abortable(site, rx) {
                    Some(Ok(s)) => s,
                    Some(Err(e)) => {
                        elog!("synthesis failed: {e:#}");
                        continue;
                    }
                    None => return Ok(()),
                };
                let waited = wait_start.elapsed();
                if first_audio {
                    first_audio = false;
                    match prev_end {
                        Some((t, queued)) if t.elapsed() > queued + Duration::from_millis(20) => elog!(
                            "first audio after {} ms: ~{} ms gap after the previous Speak",
                            t_start.elapsed().as_millis(),
                            (t.elapsed() - queued).as_millis()
                        ),
                        _ => elog!("first audio after {} ms", t_start.elapsed().as_millis()),
                    }
                } else if waited > buffered + Duration::from_millis(20) {
                    elog!(
                        "UNDERRUN: waited {} ms for synthesis with {} ms buffered: ~{} ms gap",
                        waited.as_millis(),
                        buffered.as_millis(),
                        (waited - buffered).as_millis()
                    );
                }
                if waited > buffered {
                    // Playback ran dry while we waited. Restart the clock so
                    // the next writes don't burst to "catch up" (which would
                    // buffer seconds of audio and make pause lag).
                    pacer.reset();
                }

                let pcm = to_pcm16(trim_silence(&samples, sentence.mid_sentence), volume as f32 / 100.0);
                let events = word_boundaries(&sentence, src_offset, pcm.len());
                if !write_with_events(site, &pcm, events, &mut bytes_written, &mut pacer) {
                    return Ok(());
                }
            }
        }
    }
    *queued_at_end = Some(pacer.lead(bytes_written));
    Ok(())
}

/// Wait for the next synthesized chunk, checking for abort every 50 ms.
/// None = abort, or the worker is gone.
fn recv_abortable(site: &ISpTTSEngineSite, rx: &Receiver<Chunk>) -> Option<Chunk> {
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(r) => return Some(r),
            Err(RecvTimeoutError::Timeout) => {
                if unsafe { site.GetActions() } & SPVES_ABORT != 0 {
                    elog!("abort requested while waiting for synthesis");
                    return None;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

/// Finished chunks the worker may queue ahead of playback. With one queued
/// and one in progress the worker never idles while there is text left, which
/// is all that matters when synthesis is near real time.
const LOOKAHEAD: usize = 1;

/// Synthesize `texts` in order on a worker thread. The worker stops when the
/// receiver is dropped or `shared` is cancelled.
fn spawn_producer(texts: Vec<String>, voice: String, assets: PathBuf, shared: Arc<Shared>) -> Receiver<Chunk> {
    let (tx, rx) = sync_channel(LOOKAHEAD);
    std::thread::spawn(move || {
        let synth = shared_synth(&assets);
        for text in texts {
            if shared.cancelled() {
                return;
            }
            let speed = speed_for(shared.rate_adj.load(Ordering::SeqCst));
            let result = match &synth {
                Some(s) => {
                    // Another Speak's worker may still hold this briefly
                    // while its terminated inference unwinds.
                    let mut s = s.lock().unwrap_or_else(|e| e.into_inner());
                    if shared.cancelled() {
                        return;
                    }
                    s.synthesize_with(&text, &voice, speed, shared.run_opts.as_ref())
                }
                None => Ok(test_tone(&text)),
            };
            if shared.cancelled() || tx.send(result).is_err() {
                return;
            }
        }
    });
    rx
}

/// The process-wide model. Loading takes seconds (and ~400 MB), and hosts
/// create a fresh engine object per voice, so it is loaded once and shared.
/// Concurrent callers block until the first load finishes. None = assets
/// missing; the engine then falls back to a test tone.
fn shared_synth(assets: &Path) -> Option<Arc<Mutex<Synth>>> {
    static SYNTH: OnceLock<Option<Arc<Mutex<Synth>>>> = OnceLock::new();
    SYNTH
        .get_or_init(|| {
            let t = Instant::now();
            crate::synth::exempt_thread_from_power_throttling();
            match Synth::new(assets) {
                Ok(mut s) => {
                    // The very first inference pays one-off setup costs; take
                    // them here. (Measured: a sentence-length warm-up costs
                    // ~6 s and doesn't make the next run any faster, so keep
                    // it tiny.)
                    if let Err(e) = s.synthesize("Hello there.", "af_heart", 1.0) {
                        elog!("warm-up failed: {e:#}");
                    }
                    elog!("synth ready (load + warm-up) in {} ms", t.elapsed().as_millis());
                    Some(Arc::new(Mutex::new(s)))
                }
                Err(e) => {
                    elog!("Synth init failed ({e:#}); falling back to test tone. Run setup.exe install.");
                    None
                }
            }
        })
        .clone()
}

/// Fallback so the COM/SAPI plumbing can be smoke-tested without assets:
/// a 440 Hz tone, ~80 ms per word.
fn test_tone(text: &str) -> Vec<f32> {
    let words = text.split_whitespace().count().max(1);
    let n = words * 80 * SAMPLE_RATE as usize / 1000;
    (0..n)
        .map(|i| (i as f32 * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32).sin() * 0.25)
        .collect()
}

/// ~100 ms of audio per Write. Small chunks give us a GetActions check every
/// ~100 ms, and a purge unblocks a pending Write with an error.
const CHUNK_BYTES: usize = (SAMPLE_RATE as usize / 10) * 2;

/// Max audio we keep buffered ahead of real-time playback. ISpVoice::Pause
/// does NOT stop the audio device — it only blocks our Write calls, and
/// everything already buffered plays out (measured: pause took effect seconds
/// late, at the end of the sentence). Since we synthesize much faster than
/// real-time, unthrottled writes buffer the whole sentence and pause feels
/// broken. Pacing writes to playback speed + this lead caps the pause delay.
const MAX_LEAD: Duration = Duration::from_millis(250);

/// Lead allowed while writing the last chunk of a Speak call. Readers often
/// issue one Speak per sentence, and SAPI only hands us the next sentence
/// once this Speak returns — so with a 250 ms lead the next sentence's
/// synthesis (1–2 s) is pure silence. Running further ahead on the final
/// chunk lets Speak return early and the next one synthesize while this one
/// is still playing. The cost: an ISpVoice::Pause during the last moments of
/// a call takes up to this long to be heard (stop/purge is still immediate).
/// Overridable per voice with the `TailLeadMs` token value.
const DEFAULT_TAIL_LEAD_MS: u32 = 1500;

/// Tracks wall-clock vs written audio so writes stay ~MAX_LEAD ahead of
/// playback. The clock starts at the first write (and restarts after an
/// underrun, from the position written so far).
struct Pacer {
    start: Option<Instant>,
    /// Byte position the clock (re)started at.
    origin: u64,
    reset_pending: bool,
    max_lead: Duration,
}

impl Pacer {
    fn new() -> Self {
        Self {
            start: None,
            origin: 0,
            reset_pending: false,
            max_lead: MAX_LEAD,
        }
    }

    fn audio_since_origin(&self, bytes_written: u64) -> Duration {
        Duration::from_secs_f64(bytes_written.saturating_sub(self.origin) as f64 / (SAMPLE_RATE * 2) as f64)
    }

    /// Sleep until `bytes_written` is at most MAX_LEAD ahead of playback.
    /// Returns false on abort.
    fn throttle(&mut self, site: &ISpTTSEngineSite, bytes_written: u64) -> bool {
        if self.start.is_none() {
            self.start = Some(Instant::now());
            self.origin = if self.reset_pending { bytes_written } else { 0 };
            self.reset_pending = false;
        }
        let start = self.start.unwrap();
        let audio_pos = self.audio_since_origin(bytes_written);
        loop {
            if unsafe { site.GetActions() } & SPVES_ABORT != 0 {
                elog!("abort requested while pacing");
                return false;
            }
            let elapsed = start.elapsed();
            if audio_pos <= elapsed + self.max_lead {
                return true;
            }
            std::thread::sleep((audio_pos - elapsed - self.max_lead).min(Duration::from_millis(50)));
        }
    }

    /// How much written audio has not been played yet (approximately).
    fn lead(&self, bytes_written: u64) -> Duration {
        match self.start {
            Some(start) => self.audio_since_origin(bytes_written).saturating_sub(start.elapsed()),
            None => Duration::ZERO,
        }
    }

    /// Playback ran dry: the next write restarts the clock at that position.
    fn reset(&mut self) {
        self.start = None;
        self.reset_pending = true;
    }

    /// A Write that blocked for long means SAPI held us (pause, or a full
    /// device buffer): playback did not consume audio during that time, so
    /// shift the clock to keep the lead at MAX_LEAD instead of bursting.
    fn on_blocked_write(&mut self, blocked_for: Duration) {
        if let Some(start) = self.start.as_mut() {
            *start += blocked_for;
        }
    }
}

/// Returns false when speaking should stop (abort/purge) — not an error.
fn write_pcm(site: &ISpTTSEngineSite, data: &[u8], bytes_written: &mut u64, pacer: &mut Pacer) -> bool {
    for chunk in data.chunks(CHUNK_BYTES) {
        if !pacer.throttle(site, *bytes_written) {
            return false;
        }
        if unsafe { site.GetActions() } & SPVES_ABORT != 0 {
            elog!("abort requested mid-write");
            return false;
        }
        let before = Instant::now();
        if let Err(e) = unsafe { site.Write(chunk.as_ptr() as *const c_void, chunk.len() as u32) } {
            elog!("write interrupted (purge): {e}");
            return false;
        }
        let blocked = before.elapsed();
        if blocked > Duration::from_millis(250) {
            pacer.on_blocked_write(blocked);
        }
        *bytes_written += chunk.len() as u64;
    }
    true
}

unsafe fn collect_frags(mut cur: *const SPVTEXTFRAG) -> Vec<Frag> {
    let mut out = Vec::new();
    while !cur.is_null() {
        let f = &*cur;
        let action = f.State.eAction;
        if action == SPVA_Speak || action == SPVA_Pronounce || action == SPVA_SpellOut {
            if !f.pTextStart.is_null() && f.ulTextLen > 0 {
                let slice = std::slice::from_raw_parts(f.pTextStart.0, f.ulTextLen as usize);
                out.push(Frag::Text {
                    utf16: slice.to_vec(),
                    src_offset: f.ulTextSrcOffset,
                });
            }
        } else if action == SPVA_Silence {
            out.push(Frag::SilenceMs(f.State.SilenceMSecs));
        }
        cur = f.pNext;
    }
    out
}

struct Sentence {
    text: String,
    /// (utf16_offset_in_frag, utf16_len, char_start_in_sentence) per word
    words: Vec<(u32, u32, usize)>,
    char_len: usize,
    /// Split at a clause break (or the length limit), not a sentence end:
    /// the pause after it should be short.
    mid_sentence: bool,
}

/// A clause break (comma, dash) ends a chunk once the chunk has at least this
/// many chars. Synthesis runs near real time on laptop CPUs, so a chunk's
/// length is directly how long playback waits before it can start. Kokoro
/// gets the comma as the chunk's final token, so it keeps a continuation
/// intonation rather than a sentence-final drop.
const CLAUSE_MIN_CHARS: usize = 80;
/// Same, for the first chunk of a Speak call: the one the listener waits on.
const FIRST_CLAUSE_MIN_CHARS: usize = 25;
const SECOND_CLAUSE_MIN_CHARS: usize = 30;
/// Never split off a sentence tail shorter than this; it sounds clipped.
const MIN_TAIL_CHARS: usize = 15;
/// No punctuation at all for this long: split at a space. Keeps chunks well
/// under Kokoro's 510-token context, which would otherwise cut the audio off.
const MAX_CHUNK_CHARS: usize = 250;

fn is_terminator(c: char) -> bool {
    matches!(c, '.' | '!' | '?' | '…' | ';' | ':')
}

/// Split into chunks on sentence-final punctuation, keeping the terminator
/// with the chunk (Kokoro uses it for prosody); long sentences are split
/// further at clause breaks. Tracks UTF-16 offsets for events.
fn split_sentences(text: &str, first_of_call: bool) -> Vec<Sentence> {
    let chars: Vec<char> = text.chars().collect();
    let mut sentences = Vec::new();
    let mut start_c = 0usize; // char index of the current chunk's start
    for i in 0..chars.len() {
        let c = chars[i];
        let len = i + 1 - start_c;
        // Grow chunks gradually at the start of a call: chunk N+1 is
        // synthesized while chunk N plays, so a short chunk followed by a
        // long one would run dry.
        let min_clause = match (first_of_call, sentences.len()) {
            (true, 0) => FIRST_CLAUSE_MIN_CHARS,
            (true, 1) => SECOND_CLAUSE_MIN_CHARS,
            _ => CLAUSE_MIN_CHARS,
        };
        let clause_break = matches!(c, ',' | '—' | '–') && len >= min_clause && {
            // Alphanumeric chars left in this sentence after the break.
            let tail = chars[i + 1..]
                .iter()
                .take_while(|&&c| !is_terminator(c))
                .filter(|c| c.is_alphanumeric())
                .count();
            tail >= MIN_TAIL_CHARS
        };
        let is_end = is_terminator(c)
            || i == chars.len() - 1
            || clause_break
            || (c.is_whitespace() && len >= MAX_CHUNK_CHARS);
        if is_end {
            let end = i + 1;
            let mid_sentence = !is_terminator(c) && i != chars.len() - 1;
            if chars[start_c..end].iter().any(|c| c.is_alphanumeric()) {
                sentences.push(make_sentence(&chars, start_c, end, mid_sentence));
            }
            start_c = end;
        }
    }
    sentences
}

fn make_sentence(chars: &[char], start: usize, end: usize, mid_sentence: bool) -> Sentence {
    // utf16 offset of `start` within the fragment
    let utf16_before: usize = chars[..start].iter().map(|c| c.len_utf16()).sum();
    let mut words = Vec::new();
    let mut u16_off = utf16_before;
    let mut j = start;
    while j < end {
        if chars[j].is_whitespace() {
            u16_off += chars[j].len_utf16();
            j += 1;
            continue;
        }
        let word_start_u16 = u16_off;
        let word_start_char = j - start;
        let mut word_u16_len = 0usize;
        while j < end && !chars[j].is_whitespace() {
            word_u16_len += chars[j].len_utf16();
            u16_off += chars[j].len_utf16();
            j += 1;
        }
        words.push((word_start_u16 as u32, word_u16_len as u32, word_start_char));
    }
    Sentence {
        text: chars[start..end].iter().collect::<String>().trim().to_string(),
        words,
        char_len: end - start,
        mid_sentence,
    }
}

/// Kokoro pads every chunk with ~300 ms of leading and ~450 ms of trailing
/// silence (measured). Between sentences the trailing pad is the natural
/// pause, but the leading pad only adds to it, and to any wait for synthesis
/// between Speak calls; at a clause split both pads would make a ~750 ms
/// stall mid-sentence. Keep a little of each so onsets aren't clipped.
const KEEP_LEAD_MS: usize = 60;
const KEEP_TAIL_MID_SENTENCE_MS: usize = 150;
const SILENCE_THRESHOLD: f32 = 0.01;

fn trim_silence(samples: &[f32], mid_sentence: bool) -> &[f32] {
    let per_ms = SAMPLE_RATE as usize / 1000;
    let Some(first) = samples.iter().position(|v| v.abs() > SILENCE_THRESHOLD) else {
        return samples;
    };
    let start = first.saturating_sub(KEEP_LEAD_MS * per_ms);
    let mut end = samples.len();
    if mid_sentence {
        let last = samples.iter().rposition(|v| v.abs() > SILENCE_THRESHOLD).unwrap_or(end);
        end = (last + 1 + KEEP_TAIL_MID_SENTENCE_MS * per_ms).min(samples.len());
    }
    &samples[start..end]
}

/// SPEI_WORD_BOUNDARY events for a sentence, each paired with its byte offset
/// into the sentence's PCM (estimated proportionally to character position).
/// These drive word highlighting in Chrome's reading mode, Logos and others.
fn word_boundaries(sentence: &Sentence, src_offset: u32, pcm_len: usize) -> Vec<(usize, SPEVENT)> {
    if sentence.char_len == 0 {
        return Vec::new();
    }
    sentence
        .words
        .iter()
        .map(|&(u16_off, u16_len, char_start)| {
            let pcm_off = (pcm_len * char_start / sentence.char_len) & !1;
            let mut ev: SPEVENT = unsafe { std::mem::zeroed() };
            ev._bitfield = SPEI_WORD_BOUNDARY.0; // eEventId:16 | elParamType:16 (UNDEFINED=0)
            ev.wParam = WPARAM(u16_len as usize);
            ev.lParam = LPARAM((src_offset + u16_off) as isize);
            (pcm_off, ev)
        })
        .collect()
}

/// Write a sentence's audio, adding each event right before the audio it
/// belongs to. Native SAPI honours ullAudioStreamOffset, but System.Speech
/// (used by .NET hosts) fires an event at the write position it was added at:
/// queuing a whole sentence's events up front made every word fire at once
/// when the sentence started. Returns false on abort, like write_pcm.
fn write_with_events(
    site: &ISpTTSEngineSite,
    pcm: &[u8],
    events: Vec<(usize, SPEVENT)>,
    bytes_written: &mut u64,
    pacer: &mut Pacer,
) -> bool {
    let mut pos = 0;
    for (off, mut ev) in events {
        let off = off.clamp(pos, pcm.len());
        if !write_pcm(site, &pcm[pos..off], bytes_written, pacer) {
            return false;
        }
        pos = off;
        ev.ullAudioStreamOffset = *bytes_written;
        if let Err(e) = unsafe { site.AddEvents(&ev, 1) } {
            elog!("AddEvents failed: {e}");
        }
    }
    write_pcm(site, &pcm[pos..], bytes_written, pacer)
}

fn to_pcm16(samples: &[f32], gain: f32) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let v = (s * gain).clamp(-1.0, 1.0);
        out.extend_from_slice(&((v * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str, first: bool) -> Vec<String> {
        split_sentences(s, first).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn splits_on_sentence_terminators() {
        assert_eq!(texts("One two. Three four! Five?", false), ["One two.", "Three four!", "Five?"]);
    }

    #[test]
    fn first_chunk_splits_early_at_a_comma() {
        let s = "Now the earth was formless and empty, darkness was over the surface of the deep.";
        assert_eq!(
            texts(s, true),
            ["Now the earth was formless and empty,", "darkness was over the surface of the deep."]
        );
        // Not the first chunk of the call: 37 chars is below the clause limit.
        assert_eq!(texts(s, false), [s]);
    }

    #[test]
    fn marks_clause_splits_as_mid_sentence() {
        let s = "Now the earth was formless and empty, darkness was over the surface of the deep. Then";
        let flags: Vec<bool> = split_sentences(s, true).iter().map(|c| c.mid_sentence).collect();
        assert_eq!(flags, [true, false, false]);
    }

    #[test]
    fn trims_leading_pad_always_and_trailing_pad_only_mid_sentence() {
        let ms = |n: usize| n * SAMPLE_RATE as usize / 1000;
        let mut audio = vec![0.0f32; ms(300)];
        audio.extend(std::iter::repeat(0.5).take(ms(1000)));
        audio.extend(std::iter::repeat(0.0).take(ms(450)));
        assert_eq!(trim_silence(&audio, false).len(), ms(60) + ms(1000) + ms(450));
        assert_eq!(trim_silence(&audio, true).len(), ms(60) + ms(1000) + ms(150));
    }

    #[test]
    fn keeps_short_tail_attached() {
        assert_eq!(
            texts("Now the earth was formless and empty, he said.", true),
            ["Now the earth was formless and empty, he said."]
        );
    }

    #[test]
    fn word_boundaries_spread_across_the_audio_in_order() {
        let s = &split_sentences("In the beginning God created", false)[0];
        let offs: Vec<usize> = word_boundaries(s, 0, 2800).iter().map(|(o, _)| *o).collect();
        assert_eq!(offs, [0, 300, 700, 1700, 2100]);
    }

    #[test]
    fn word_offsets_survive_clause_split() {
        let s = "Now the earth was formless and empty, darkness was over the surface of the deep.";
        let chunks = split_sentences(s, true);
        let (off, len, _) = chunks[1].words[0];
        let utf16: Vec<u16> = s.encode_utf16().collect();
        assert_eq!(String::from_utf16_lossy(&utf16[off as usize..(off + len) as usize]), "darkness");
    }
}
