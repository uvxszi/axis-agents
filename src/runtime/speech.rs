//! Speech-to-text and text-to-speech for voice channels.
//!
//! Local engines run in one long-lived Python helper (faster-whisper for
//! hearing, Piper for speaking) so models load once; Windows voices and
//! OpenAI-compatible APIs need no Python at all.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

use crate::config::{SttEngine, TtsEngine, VoiceConfig};

const HELPER_SCRIPT: &str = r#"
import sys, json, os, wave, importlib.util
from pathlib import Path

out = sys.stdout
sys.stdout = sys.stderr  # libraries' prints must not corrupt the protocol

# Let CTranslate2 find the CUDA libraries that ship with PyTorch.
try:
    spec = importlib.util.find_spec("torch")
    if spec and spec.origin and hasattr(os, "add_dll_directory"):
        lib = Path(spec.origin).parent / "lib"
        if lib.exists():
            os.add_dll_directory(str(lib))
except Exception:
    pass

whisper = {}
voices = {}
state = {"device": None}

def load_whisper(name, device):
    key = (name, device)
    if key not in whisper:
        from faster_whisper import WhisperModel
        if device == "cpu":
            whisper[key] = WhisperModel(name, device="cpu", compute_type="int8")
        else:
            whisper[key] = WhisperModel(name, device="cuda", compute_type="float16")
    return whisper[key]

def stt(req):
    name = req.get("model") or "base"
    lang = req.get("language") or None
    devices = [state["device"]] if state["device"] else ["cuda", "cpu"]
    last = None
    for device in devices:
        try:
            model = load_whisper(name, device)
            segments, info = model.transcribe(
                req["wav"], language=lang, beam_size=1, vad_filter=True,
                condition_on_previous_text=False, hotwords=req.get("hotwords") or None,
                initial_prompt=req.get("context") or None)
            text = " ".join(s.text.strip() for s in segments).strip()
            state["device"] = device
            return {"ok": True, "text": text, "language": info.language, "device": device}
        except Exception as e:
            last = e
            whisper.pop((name, device), None)
    raise last

def load_voice(name, data_dir):
    key = (name, data_dir)
    if key not in voices:
        from piper import PiperVoice
        d = Path(data_dir)
        d.mkdir(parents=True, exist_ok=True)
        model = d / (name + ".onnx")
        if not model.exists() or not (d / (name + ".onnx.json")).exists():
            from piper.download_voices import download_voice
            download_voice(name, d)
        voices[key] = PiperVoice.load(str(model))
    return voices[key]

def tts(req):
    from piper import SynthesisConfig
    voice = load_voice(req["voice"], req["data_dir"])
    speed = max(0.3, float(req.get("speed") or 1.0))
    cfg = SynthesisConfig(length_scale=1.0 / speed, volume=float(req.get("volume") or 1.0))
    with wave.open(req["out"], "wb") as w:
        voice.synthesize_wav(req["text"], w, syn_config=cfg)
    return {"ok": True}

kokoro = {}

def fetch(url, path):
    import urllib.request
    tmp = str(path) + ".part"
    urllib.request.urlretrieve(url, tmp)
    os.replace(tmp, path)

def load_kokoro(data_dir):
    if data_dir not in kokoro:
        from kokoro_onnx import Kokoro
        d = Path(data_dir)
        d.mkdir(parents=True, exist_ok=True)
        base = "https://github.com/thewh1teagle/kokoro-onnx/releases/download/model-files-v1.0/"
        model, voices = d / "kokoro-v1.0.int8.onnx", d / "voices-v1.0.bin"
        for f in (model, voices):
            if not f.exists():
                fetch(base + f.name, f)
        kokoro[data_dir] = Kokoro(str(model), str(voices))
    return kokoro[data_dir]

def tts_kokoro(req):
    import numpy as np
    k = load_kokoro(req["data_dir"])
    voice = req.get("voice") or "af_heart"
    lang = {"a": "en-us", "b": "en-gb", "e": "es", "f": "fr-fr", "h": "hi", "i": "it", "p": "pt-br", "j": "ja", "z": "cmn"}.get(voice[:1], "en-us")
    samples, rate = k.create(req["text"], voice=voice, speed=max(0.5, float(req.get("speed") or 1.0)), lang=lang)
    samples = np.clip(samples * float(req.get("volume") or 1.0), -1.0, 1.0)
    with wave.open(req["out"], "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(rate)
        w.writeframes((samples * 32767).astype("<i2").tobytes())
    return {"ok": True}

def check(req):
    r = {"ok": True, "python": sys.version.split()[0]}
    for mod, key in (("faster_whisper", "whisper"), ("piper", "piper"), ("kokoro_onnx", "kokoro")):
        r[key] = importlib.util.find_spec(mod) is not None
    return r

ops = {"stt": stt, "tts": tts, "kokoro": tts_kokoro, "check": check}
for line in sys.stdin:
    try:
        req = json.loads(line)
        res = ops[req.get("op")](req)
    except Exception as e:
        res = {"ok": False, "error": f"{type(e).__name__}: {e}"}
    out.write(json.dumps(res) + "\n")
    out.flush()
"#;

// --- voice catalog ----------------------------------------------------------------

pub const KOKORO_VOICES: [(&str, &str); 14] = [
    ("af_heart", "Heart — US, female (best)"),
    ("af_bella", "Bella — US, female"),
    ("af_nicole", "Nicole — US, female, soft"),
    ("af_sarah", "Sarah — US, female"),
    ("af_sky", "Sky — US, female"),
    ("am_adam", "Adam — US, male"),
    ("am_michael", "Michael — US, male"),
    ("am_fenrir", "Fenrir — US, male"),
    ("am_puck", "Puck — US, male"),
    ("bf_emma", "Emma — British, female"),
    ("bf_isabella", "Isabella — British, female"),
    ("bm_george", "George — British, male"),
    ("bm_fable", "Fable — British, male"),
    ("bm_lewis", "Lewis — British, male"),
];

pub const PIPER_VOICES: [(&str, &str); 10] = [
    ("en_US-lessac-medium", "Lessac — US, neutral"),
    ("en_US-amy-medium", "Amy — US, female"),
    ("en_US-ryan-high", "Ryan — US, male"),
    ("en_US-hfc_female-medium", "HFC — US, female"),
    ("en_US-hfc_male-medium", "HFC — US, male"),
    ("en_US-joe-medium", "Joe — US, male"),
    ("en_US-kristin-medium", "Kristin — US, female"),
    ("en_GB-alan-medium", "Alan — British, male"),
    ("en_GB-jenny_dioco-medium", "Jenny — British, female"),
    (
        "en_GB-northern_english_male-medium",
        "Northern English, male",
    ),
];

/// The voices the agent can switch to: (engine, id, description).
pub fn catalog() -> Vec<(TtsEngine, &'static str, &'static str)> {
    KOKORO_VOICES
        .iter()
        .map(|(id, d)| (TtsEngine::Kokoro, *id, *d))
        .chain(
            PIPER_VOICES
                .iter()
                .map(|(id, d)| (TtsEngine::Piper, *id, *d)),
        )
        .collect()
}

/// The current voice, described.
pub fn current_voice(config: &VoiceConfig) -> String {
    let tts = &config.tts;
    let (id, engine) = match tts.engine {
        TtsEngine::Kokoro => (tts.kokoro_voice.clone(), "Kokoro"),
        TtsEngine::Piper => (tts.piper_voice.clone(), "Piper"),
        TtsEngine::Windows => (
            if tts.windows_voice.is_empty() {
                "system default".to_owned()
            } else {
                tts.windows_voice.clone()
            },
            "Windows",
        ),
        TtsEngine::OpenAi => (tts.voice.clone(), "API"),
    };
    let description = catalog()
        .into_iter()
        .find(|(e, v, _)| *e == tts.engine && *v == id)
        .map(|(_, _, d)| format!(" ({d})"))
        .unwrap_or_default();
    format!("{id}{description} via {engine}, speed {:.2}", tts.speed)
}

/// Find a voice by its ID or by description ("british male", "soft female").
pub fn find_voice(query: &str) -> Option<(TtsEngine, &'static str, &'static str)> {
    let query = query.trim().to_lowercase();
    let all = catalog();
    if let Some(exact) = all
        .iter()
        .find(|(_, id, _)| id.eq_ignore_ascii_case(&query))
    {
        return Some(*exact);
    }
    let words: Vec<&str> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .collect();
    let gender = |w: &str| match w {
        "male" | "man" | "guy" | "boy" | "masculine" | "deep" => Some("male"),
        "female" | "woman" | "girl" | "lady" | "feminine" => Some("female"),
        _ => None,
    };
    all.into_iter()
        .map(|entry| {
            let (engine, id, description) = entry;
            let text = format!("{} {description}", id.replace('_', " ")).to_lowercase();
            let tokens: Vec<&str> = text.split(|c: char| !c.is_alphanumeric()).collect();
            let mut score = 0i32;
            for word in &words {
                match gender(word) {
                    // "male" is inside "female": compare whole words.
                    Some(g) => score += if tokens.contains(&g) { 3 } else { -5 },
                    None => {
                        let word = match *word {
                            "uk" | "england" | "english" => "british",
                            "american" | "usa" => "us",
                            "gentle" | "quiet" | "calm" => "soft",
                            other => other,
                        };
                        if tokens.contains(&word) {
                            score += 2;
                        }
                    }
                }
            }
            // Prefer the more natural engine on ties.
            if engine == TtsEngine::Kokoro && score > 0 {
                score = score * 2 + 1;
            }
            (score, entry)
        })
        .filter(|(score, _)| *score > 1)
        .max_by_key(|(score, _)| *score)
        .map(|(_, entry)| entry)
}

/// A problem worth showing once (e.g. the chosen voice isn't installed).
fn notices() -> &'static std::sync::Mutex<(Option<String>, std::collections::HashSet<String>)> {
    static NOTICES: OnceLock<
        std::sync::Mutex<(Option<String>, std::collections::HashSet<String>)>,
    > = OnceLock::new();
    NOTICES.get_or_init(Default::default)
}

fn note(text: String) {
    tracing::warn!("{text}");
    let mut notices = notices().lock().expect("notices poisoned");
    if notices.1.insert(text.clone()) {
        notices.0 = Some(text);
    }
}

/// Take the latest new notice, if any.
pub fn take_notice() -> Option<String> {
    notices().lock().expect("notices poisoned").0.take()
}

pub fn voice_dir() -> PathBuf {
    crate::paths::data_dir().join("voice")
}

fn temp_path(ext: &str) -> PathBuf {
    let dir = voice_dir().join("tmp");
    let _ = std::fs::create_dir_all(&dir);
    dir.join(format!("{:016x}.{ext}", fastrand::u64(..)))
}

// --- the Python helper ---------------------------------------------------------

struct Helper {
    python: String,
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

fn helper_slot() -> &'static Mutex<Option<Helper>> {
    static SLOT: OnceLock<Mutex<Option<Helper>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn command(program: &str) -> Command {
    let mut command = Command::new(program);
    command.stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x0800_0000);
    command
}

async fn spawn_helper(python: &str) -> Result<Helper, String> {
    let dir = voice_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let script = dir.join("voice_helper.py");
    std::fs::write(&script, HELPER_SCRIPT).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(dir.join("helper.log")).map_err(|e| e.to_string())?;
    let mut child = command(python)
        .arg("-u")
        .arg(&script)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(log))
        .env("PYTHONIOENCODING", "utf-8")
        .env("HF_HUB_DISABLE_SYMLINKS_WARNING", "1")
        .spawn()
        .map_err(|e| format!("couldn't start Python ({python}): {e}"))?;
    let stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
    Ok(Helper {
        python: python.to_owned(),
        child,
        stdin,
        stdout,
    })
}

/// One request to the helper (requests are serialized).
async fn helper_call(python: &str, request: Value, timeout: Duration) -> Result<Value, String> {
    let mut slot = helper_slot().lock().await;
    let alive = slot
        .as_mut()
        .is_some_and(|h| h.python == python && matches!(h.child.try_wait(), Ok(None)));
    if !alive {
        *slot = Some(spawn_helper(python).await?);
    }
    let helper = slot.as_mut().expect("helper just set");
    let line = format!("{request}\n");
    let exchange = async {
        helper
            .stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        helper.stdin.flush().await.map_err(|e| e.to_string())?;
        let mut response = String::new();
        let read = helper
            .stdout
            .read_line(&mut response)
            .await
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the Python helper exited (see voice/helper.log)".to_owned());
        }
        serde_json::from_str::<Value>(&response).map_err(|e| e.to_string())
    };
    let result = match tokio::time::timeout(timeout, exchange).await {
        Ok(result) => result,
        Err(_) => Err("the Python helper timed out".to_owned()),
    };
    if result.is_err() {
        // Out of sync or dead: start fresh next time.
        *slot = None;
    }
    let value = result?;
    if value.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(value)
    } else {
        Err(value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_owned())
    }
}

/// What the local engines need, for the settings page.
#[derive(Debug, Clone, Default)]
pub struct LocalStatus {
    pub python: Option<String>,
    pub whisper: bool,
    pub piper: bool,
    pub kokoro: bool,
    pub error: Option<String>,
}

pub async fn check_local(python: &str) -> LocalStatus {
    match helper_call(python, json!({ "op": "check" }), Duration::from_secs(30)).await {
        Ok(v) => LocalStatus {
            python: v.get("python").and_then(Value::as_str).map(str::to_owned),
            whisper: v.get("whisper").and_then(Value::as_bool).unwrap_or(false),
            piper: v.get("piper").and_then(Value::as_bool).unwrap_or(false),
            kokoro: v.get("kokoro").and_then(Value::as_bool).unwrap_or(false),
            error: None,
        },
        Err(error) => LocalStatus {
            error: Some(error),
            ..Default::default()
        },
    }
}

/// Install the local engines with pip.
pub async fn install_local(python: &str) -> Result<String, String> {
    // The helper may hold the packages open.
    *helper_slot().lock().await = None;
    let output = command(python)
        .args([
            "-m",
            "pip",
            "install",
            "--upgrade",
            "faster-whisper",
            "kokoro-onnx",
            "piper-tts",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("couldn't run pip: {e}"))?;
    if output.status.success() {
        Ok("installed faster-whisper, kokoro-onnx and piper-tts".to_owned())
    } else {
        let err = String::from_utf8_lossy(&output.stderr);
        Err(crate::llm::truncate(err.trim(), 400))
    }
}

// --- WAV ---------------------------------------------------------------------------

/// Decode a PCM WAV file to mono samples and its rate.
pub fn read_wav(bytes: &[u8]) -> Result<(Vec<i16>, u32), String> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("not a WAV file".to_owned());
    }
    let mut pos = 12;
    let (mut format, mut channels, mut rate, mut bits) = (0u16, 0u16, 0u32, 0u16);
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let mut size =
            u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap_or_default()) as usize;
        let body = pos + 8;
        // Streaming writers leave the data size unset.
        if id == b"data" && (size == 0 || size == u32::MAX as usize || body + size > bytes.len()) {
            size = bytes.len() - body;
        }
        let end = (body + size).min(bytes.len());
        if id == b"fmt " && end - body >= 16 {
            let f = &bytes[body..end];
            format = u16::from_le_bytes([f[0], f[1]]);
            channels = u16::from_le_bytes([f[2], f[3]]).max(1);
            rate = u32::from_le_bytes([f[4], f[5], f[6], f[7]]);
            bits = u16::from_le_bytes([f[14], f[15]]);
            if format == 0xFFFE && f.len() >= 26 {
                format = u16::from_le_bytes([f[24], f[25]]);
            }
        } else if id == b"data" {
            let data = &bytes[body..end];
            let frames: Vec<f32> = match (format, bits) {
                (1, 16) => data
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32)
                    .collect(),
                (3, 32) => data
                    .chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) * 32767.0)
                    .collect(),
                (1, 8) => data.iter().map(|b| (*b as f32 - 128.0) * 256.0).collect(),
                (1, 24) => data
                    .chunks_exact(3)
                    .map(|b| (i32::from_le_bytes([0, b[0], b[1], b[2]]) >> 16) as f32)
                    .collect(),
                _ => return Err(format!("unsupported WAV format {format}/{bits}-bit")),
            };
            let mono = frames
                .chunks(channels as usize)
                .map(|f| (f.iter().sum::<f32>() / f.len() as f32).clamp(-32768.0, 32767.0) as i16)
                .collect();
            if rate == 0 {
                return Err("WAV has no format".to_owned());
            }
            return Ok((mono, rate));
        }
        pos = body + size + (size & 1);
    }
    Err("WAV has no audio".to_owned())
}

/// Encode mono 16-bit samples as a WAV file.
pub fn write_wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = (samples.len() * 2) as u32;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * 2).to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// --- hearing -------------------------------------------------------------------------

/// Transcribe 16 kHz mono speech.
/// `names`: words to listen for (the agent's name and aliases), so they
/// aren't misheard. `context`: the conversation so far, which steers
/// recognition toward the words being used ("Minecraft", not "my craft").
pub async fn transcribe(
    config: &VoiceConfig,
    samples: &[i16],
    names: &[String],
    context: &str,
) -> Result<String, String> {
    let wav = write_wav(samples, 16_000);
    let text = match config.stt.engine {
        SttEngine::LocalWhisper => {
            let path = temp_path("wav");
            std::fs::write(&path, &wav).map_err(|e| e.to_string())?;
            let request = json!({
                "op": "stt",
                "wav": path.to_string_lossy(),
                "model": config.stt.whisper_model,
                "language": config.stt.language,
                "hotwords": names.join(" "),
                "context": context,
            });
            // The first call may download the model.
            let result = helper_call(&config.python, request, Duration::from_secs(600)).await;
            let _ = std::fs::remove_file(&path);
            result?
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        }
        SttEngine::OpenAi => {
            let mut form = reqwest::multipart::Form::new()
                .text("model", config.stt.model.clone())
                .part(
                    "file",
                    reqwest::multipart::Part::bytes(wav)
                        .file_name("speech.wav")
                        .mime_str("audio/wav")
                        .map_err(|e| e.to_string())?,
                );
            if !names.is_empty() || !context.is_empty() {
                form = form.text(
                    "prompt",
                    format!("{} {context}", names.join(", ")).trim().to_owned(),
                );
            }
            if !config.stt.language.trim().is_empty() {
                form = form.text("language", config.stt.language.trim().to_owned());
            }
            let url = format!(
                "{}/audio/transcriptions",
                config.stt.base_url.trim_end_matches('/')
            );
            let mut request = crate::llm::http_client()
                .post(url)
                .multipart(form)
                .timeout(Duration::from_secs(60));
            if !config.stt.api_key.trim().is_empty() {
                request = request.bearer_auth(config.stt.api_key.trim());
            }
            let response = request.send().await.map_err(|e| e.to_string())?;
            let status = response.status();
            let body: Value = response.json().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(format!("transcription failed ({status}): {body}"));
            }
            body.get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        }
    };
    Ok(clean_transcript(&text))
}

/// Whisper's stock hallucinations on noise and silence.
fn clean_transcript(text: &str) -> String {
    let text = text.trim();
    let lower = text
        .to_lowercase()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .to_owned();
    const JUNK: [&str; 12] = [
        "you",
        "thank you",
        "thanks for watching",
        "thank you for watching",
        "bye",
        "uh",
        "um",
        "hmm",
        "so",
        "subtitles by the amaraorg community",
        "please subscribe",
        "music",
    ];
    if lower.is_empty() || JUNK.contains(&lower.as_str()) {
        return String::new();
    }
    text.to_owned()
}

// --- speaking -------------------------------------------------------------------------

/// Speak text: 48 kHz interleaved stereo, ready for a voice connection.
pub async fn synthesize(config: &VoiceConfig, text: &str) -> Result<Vec<i16>, String> {
    let tts = &config.tts;
    let text = text.trim();
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = match tts.engine {
        TtsEngine::Kokoro => {
            let out = temp_path("wav");
            let request = json!({
                "op": "kokoro",
                "voice": tts.kokoro_voice.trim(),
                "text": text,
                "out": out.to_string_lossy(),
                "data_dir": voice_dir().join("kokoro").to_string_lossy(),
                "speed": tts.speed,
                "volume": tts.volume,
            });
            // The first call downloads the model (~120 MB).
            match helper_call(&config.python, request, Duration::from_secs(600)).await {
                Ok(_) => {
                    let bytes = std::fs::read(&out).map_err(|e| e.to_string());
                    let _ = std::fs::remove_file(&out);
                    bytes?
                }
                Err(error)
                    if cfg!(windows)
                        && (error.contains("No module named")
                            || error.contains("couldn't start Python")) =>
                {
                    note(format!(
                        "Kokoro isn't installed (Voice tab → Install) — using a Windows voice for now. {error}"
                    ));
                    windows_speak("", tts.speed, text).await?
                }
                Err(error) => return Err(error),
            }
        }
        TtsEngine::Piper => {
            let out = temp_path("wav");
            let request = json!({
                "op": "tts",
                "voice": tts.piper_voice.trim(),
                "text": text,
                "out": out.to_string_lossy(),
                "data_dir": voice_dir().join("piper").to_string_lossy(),
                "speed": tts.speed,
                "volume": tts.volume,
            });
            match helper_call(&config.python, request, Duration::from_secs(300)).await {
                Ok(_) => {
                    let bytes = std::fs::read(&out).map_err(|e| e.to_string());
                    let _ = std::fs::remove_file(&out);
                    bytes?
                }
                // Not installed yet (Voice tab → Install): talk with a
                // Windows voice meanwhile rather than not at all.
                Err(error)
                    if cfg!(windows)
                        && (error.contains("No module named")
                            || error.contains("couldn't start Python")) =>
                {
                    note(format!(
                        "Piper isn't installed (Voice tab → Install) — using a Windows voice for now. {error}"
                    ));
                    windows_speak("", tts.speed, text).await?
                }
                Err(error) => return Err(error),
            }
        }
        TtsEngine::Windows => windows_speak(tts.windows_voice.trim(), tts.speed, text).await?,
        TtsEngine::OpenAi => {
            let url = format!("{}/audio/speech", tts.base_url.trim_end_matches('/'));
            let mut request = crate::llm::http_client()
                .post(url)
                .json(&json!({
                    "model": tts.model,
                    "voice": tts.voice,
                    "input": text,
                    "response_format": "wav",
                    "speed": tts.speed,
                }))
                .timeout(Duration::from_secs(120));
            if !tts.api_key.trim().is_empty() {
                request = request.bearer_auth(tts.api_key.trim());
            }
            let response = request.send().await.map_err(|e| e.to_string())?;
            let status = response.status();
            let bytes = response.bytes().await.map_err(|e| e.to_string())?;
            if !status.is_success() {
                return Err(format!(
                    "speech failed ({status}): {}",
                    crate::llm::truncate(&String::from_utf8_lossy(&bytes), 200)
                ));
            }
            bytes.to_vec()
        }
    };
    let (mono, rate) = read_wav(&bytes)?;
    let volume = if matches!(tts.engine, TtsEngine::Piper | TtsEngine::Kokoro) {
        1.0
    } else {
        tts.volume.clamp(0.1, 2.0)
    };
    let mono: Vec<i16> = mono
        .into_iter()
        .map(|s| (s as f32 * volume).clamp(-32768.0, 32767.0) as i16)
        .collect();
    Ok(axis_voice::from_mono(&mono, rate))
}

#[cfg(windows)]
async fn windows_speak(voice: &str, speed: f32, text: &str) -> Result<Vec<u8>, String> {
    let out = temp_path("wav");
    let input = temp_path("txt");
    std::fs::write(&input, text).map_err(|e| e.to_string())?;
    let rate = (((speed - 1.0) * 10.0).round() as i32).clamp(-10, 10);
    let select = if voice.is_empty() {
        String::new()
    } else {
        format!("$s.SelectVoice('{}');", voice.replace('\'', "''"))
    };
    let script = format!(
        "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; {select} $s.Rate = {rate}; \
         $s.SetOutputToWaveFile('{out}'); $s.Speak([IO.File]::ReadAllText('{input}', [Text.Encoding]::UTF8)); $s.Dispose()",
        out = out.to_string_lossy().replace('\'', "''"),
        input = input.to_string_lossy().replace('\'', "''"),
    );
    let output = command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&input);
    let output = output?;
    let bytes = std::fs::read(&out);
    let _ = std::fs::remove_file(&out);
    match bytes {
        Ok(bytes) if output.status.success() => Ok(bytes),
        _ => Err(format!(
            "Windows speech failed: {}",
            crate::llm::truncate(String::from_utf8_lossy(&output.stderr).trim(), 300)
        )),
    }
}

#[cfg(not(windows))]
async fn windows_speak(_voice: &str, _speed: f32, _text: &str) -> Result<Vec<u8>, String> {
    Err("Windows voices are only available on Windows".to_owned())
}

/// The voices installed in Windows.
pub async fn windows_voices() -> Vec<String> {
    if !cfg!(windows) {
        return Vec::new();
    }
    let script = "Add-Type -AssemblyName System.Speech; (New-Object System.Speech.Synthesis.SpeechSynthesizer).GetInstalledVoices() | ForEach-Object { $_.VoiceInfo.Name }";
    match command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .await
    {
        Ok(output) => String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Play speech on this PC's speakers (the settings page preview).
pub async fn preview(config: &VoiceConfig, text: &str) -> Result<(), String> {
    let stereo = synthesize(config, text).await?;
    let mono = axis_voice::to_mono(&stereo, 48_000);
    let path = temp_path("wav");
    std::fs::write(&path, write_wav(&mono, 48_000)).map_err(|e| e.to_string())?;
    let script = format!(
        "(New-Object Media.SoundPlayer '{}').PlaySync()",
        path.to_string_lossy().replace('\'', "''")
    );
    let result = command("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .await
        .map_err(|e| e.to_string());
    let _ = std::fs::remove_file(&path);
    result.map(|_| ())
}

/// Text as it should be spoken: no markdown, links, emoji codes, or
/// mention syntax.
pub fn speakable(text: &str) -> String {
    let text = drop_ids(&markdown_links(text));
    let mut out = String::with_capacity(text.len());
    for word in text.split_whitespace() {
        // Links can't be read aloud; the sentence around them says enough.
        if word.contains("http://") || word.contains("https://") {
            continue;
        }
        // Custom emoji <:name:id> / <a:name:id>, mentions <@id> <#id>.
        if word.starts_with('<') && word.contains('>') {
            continue;
        }
        let cleaned: String = word
            .chars()
            .filter(|c| !matches!(c, '*' | '_' | '`' | '#' | '~' | '|' | '>'))
            .filter(|c| !is_emoji(*c))
            .collect();
        if !cleaned.is_empty() {
            out.push_str(&cleaned);
            out.push(' ');
        }
    }
    out.trim().to_owned()
}

/// `[text](url)` → text, `![alt](url)` → nothing.
fn markdown_links(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let image = open > 0 && rest[..open].ends_with('!');
        let Some(close) = rest[open..].find("](") else {
            break;
        };
        let Some(end) = rest[open + close..].find(')') else {
            break;
        };
        out.push_str(&rest[..if image { open - 1 } else { open }]);
        if !image {
            out.push_str(&rest[open + 1..open + close]);
        }
        rest = &rest[open + close + end + 1..];
    }
    out.push_str(rest);
    out
}

/// Discord IDs (and the "(message 1234…)" asides around them) are
/// noise when read aloud.
fn drop_ids(text: &str) -> String {
    let has_id = |t: &str| {
        let mut run = 0;
        t.chars().any(|c| {
            run = if c.is_ascii_digit() { run + 1 } else { 0 };
            run >= 15
        })
    };
    // Parenthesized asides that carry an ID go entirely.
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('(') {
        let Some(close) = rest[open..].find(')') else {
            break;
        };
        let aside = &rest[open..open + close + 1];
        out.push_str(&rest[..open]);
        if !has_id(aside) {
            out.push_str(aside);
        }
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    // Then any ID left in the sentence, with its "ID"/"message" label.
    let mut kept: Vec<String> = Vec::new();
    for word in out.split_whitespace() {
        if !has_id(word) {
            kept.push(word.to_owned());
            continue;
        }
        while kept.last().is_some_and(|w| {
            matches!(
                w.to_lowercase()
                    .trim_matches(|c: char| !c.is_alphanumeric()),
                "id" | "message" | "msg" | "number"
            )
        }) {
            kept.pop();
        }
        // Keep the sentence's punctuation.
        if let Some(end) = word
            .chars()
            .last()
            .filter(|c| matches!(c, '.' | '!' | '?' | ','))
        {
            if let Some(last) = kept.last_mut() {
                last.push(end);
            }
        }
    }
    kept.join(" ").replace(" .", ".").replace(" ,", ",")
}

fn is_emoji(c: char) -> bool {
    matches!(c as u32, 0x1F000..=0x1FAFF | 0x2600..=0x27BF | 0xFE0F | 0x200D)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_round_trip() {
        let samples: Vec<i16> = (0..1600).map(|i| (i * 7 % 2000) as i16 - 1000).collect();
        let (back, rate) = read_wav(&write_wav(&samples, 16_000)).unwrap();
        assert_eq!(rate, 16_000);
        assert_eq!(back, samples);
    }

    #[test]
    fn speakable_strips_markup() {
        assert_eq!(
            speakable("**hey** <@123> check https://x.y/z 😂 it's `cool`"),
            "hey check it's cool"
        );
        assert_eq!(
            speakable(
                "Here's a zebra: ![Plains Zebra](https://x/z.jpg) from [Wikimedia](https://commons.org)."
            ),
            "Here's a zebra: from Wikimedia."
        );
        assert_eq!(speakable("<:yinyang:1566329537850253383> ok"), "ok");
        assert_eq!(
            speakable("Done — sent to scratchpad3 (message 1566358877392474205)."),
            "Done — sent to scratchpad3."
        );
        assert_eq!(
            speakable("Deleted message ID 1566358877392474205, all good"),
            "Deleted, all good"
        );
        assert_eq!(
            speakable("Version 0.0.6 (out now) is 12 MB"),
            "Version 0.0.6 (out now) is 12 MB"
        );
    }

    /// Windows voice → local Whisper: `cargo test live_speech -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_speech_round_trip() {
        let mut config = VoiceConfig::default();
        config.tts.engine = TtsEngine::Windows;
        let started = std::time::Instant::now();
        let stereo = synthesize(&config, "Hello Magi, please join the gaming voice channel.")
            .await
            .unwrap();
        println!(
            "tts: {:.1}s of audio in {:.1}s",
            stereo.len() as f32 / 96_000.0,
            started.elapsed().as_secs_f32()
        );
        let mono = axis_voice::to_mono(&stereo, 16_000);
        for _ in 0..2 {
            let started = std::time::Instant::now();
            let text = transcribe(&config, &mono, &["Magi".to_owned()], "")
                .await
                .unwrap();
            println!("stt: {text:?} in {:.1}s", started.elapsed().as_secs_f32());
            assert!(text.to_lowercase().contains("gaming"), "{text}");
            assert!(
                crate::runtime::voice::heard_name(&text, &["Magi".to_owned()]),
                "{text}"
            );
        }
    }

    /// Kokoro → local Whisper: `cargo test live_kokoro -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_kokoro_round_trip() {
        let config = VoiceConfig::default();
        assert_eq!(config.tts.engine, TtsEngine::Kokoro);
        for _ in 0..2 {
            let started = std::time::Instant::now();
            let stereo = synthesize(&config, "Hey, it's Magi. I joined the gaming channel.")
                .await
                .unwrap();
            println!(
                "kokoro: {:.1}s of audio in {:.1}s",
                stereo.len() as f32 / 96_000.0,
                started.elapsed().as_secs_f32()
            );
            assert!(take_notice().is_none(), "fell back to Windows");
            let text = transcribe(
                &config,
                &axis_voice::to_mono(&stereo, 16_000),
                &["Magi".to_owned()],
                "",
            )
            .await
            .unwrap();
            println!("heard: {text:?}");
            assert!(text.to_lowercase().contains("gaming"), "{text}");
        }
    }

    #[test]
    fn finds_voices_by_description() {
        assert_eq!(find_voice("bm_george").unwrap().1, "bm_george");
        let (_, id, _) = find_voice("british male").unwrap();
        assert!(id.starts_with("bm_"), "{id}");
        let (_, id, _) = find_voice("a soft female voice").unwrap();
        assert_eq!(id, "af_nicole");
        let (_, id, _) = find_voice("american guy").unwrap();
        assert!(id.starts_with("am_"), "{id}");
        assert!(find_voice("zzz").is_none());
    }

    #[test]
    fn whisper_junk_is_dropped() {
        assert_eq!(clean_transcript(" Thank you. "), "");
        assert_eq!(clean_transcript("you"), "");
        assert_eq!(clean_transcript("Magi, what's up?"), "Magi, what's up?");
    }
}
