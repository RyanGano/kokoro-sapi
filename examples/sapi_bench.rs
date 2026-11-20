//! End-to-end timing through native SAPI, like a reader app: queues a few
//! Speak calls asynchronously and reports the elapsed time against the audio
//! produced. Volume is 0 so nothing is audible; the engine's own timing lines
//! (first audio, underruns, per-chunk RTF) go to engine.log.
//!
//!   cargo run --release --example sapi_bench [TOKEN] [per-sentence]
//!
//! `per-sentence` issues one Speak per sentence (how some readers drive TTS)
//! instead of one per paragraph.

use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Media::Speech::{
    ISpObjectToken, ISpVoice, SpObjectToken, SpVoice, SPF_ASYNC, SPF_DEFAULT,
};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED};

const PARAGRAPH: &[&str] = &[
    "In the beginning God created the heavens and the earth.",
    "Now the earth was formless and empty, darkness was over the surface of the deep, and the Spirit of God was hovering over the waters.",
    "And God said, Let there be light, and there was light.",
    "God saw that the light was good, and he separated the light from the darkness.",
];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn main() -> anyhow::Result<()> {
    let token = std::env::args().nth(1).unwrap_or_else(|| "KokoroHeart".into());
    let per_sentence = std::env::args().any(|a| a == "per-sentence");
    let id = wide(&format!(r"HKEY_CURRENT_USER\SOFTWARE\Microsoft\Speech\Voices\Tokens\{token}"));

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let voice: ISpVoice = CoCreateInstance(&SpVoice, None, CLSCTX_ALL)?;
        let tok: ISpObjectToken = CoCreateInstance(&SpObjectToken, None, CLSCTX_ALL)?;
        tok.SetId(PCWSTR::null(), PCWSTR(id.as_ptr()), false)?;
        let t = Instant::now();
        voice.SetVoice(&tok)?;
        voice.SetVolume(0)?;
        println!("SetVoice: {} ms", t.elapsed().as_millis());

        let t = Instant::now();
        if per_sentence {
            for s in PARAGRAPH {
                let w = wide(s);
                voice.Speak(PCWSTR(w.as_ptr()), SPF_ASYNC.0 as u32, None)?;
            }
        } else {
            let w = wide(&PARAGRAPH.join(" "));
            voice.Speak(PCWSTR(w.as_ptr()), SPF_ASYNC.0 as u32, None)?;
        }
        voice.WaitUntilDone(u32::MAX)?;
        println!("spoke paragraph in {} ms", t.elapsed().as_millis());
        let _ = SPF_DEFAULT;
    }
    Ok(())
}
