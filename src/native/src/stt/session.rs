//! Voice sessions: one mic, one listener thread, one event queue.
//!
//! Modes:
//!
//! - `manual`: press to start, VAD captures the first utterance, a silence
//!   hangover finalizes it, whisper transcribes, the session ends. Nobody
//!   presses anything to *finish*.
//! - `wake`: armed until the wake word ("hey" default, configurable) is
//!   heard in a transcribed onset utterance, then the *next* utterance is
//!   the command; re-arms until stopped.
//!
//! The frontend long-polls `events` (cursor-based, 25 s server hold) and
//! feeds final transcripts straight into the agent. Everything is local:
//! mic → VAD → tiny → text, no network after the first model download.
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;

use tokio::sync::Notify;

use super::audio::{self, TARGET_RATE};
use super::vad::{FRAME_SAMPLES, Vad, VadConfig, VadTransition};
use super::engine::Engine;
use super::model;
use super::wake::split_wake_command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenMode {
    Manual,
    Wake,
    /// Client-looped continuous talk: backend behaves like manual (one
    /// utterance → transcript → end); the renderer chains turns, TTS
    /// replies, and echo guards. Half-duplex by design (no echo
    /// cancellation in this stack): talk, hear the reply, talk again.
    Conversation,
}

impl ListenMode {
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_lowercase().as_str() {
            "manual" => Some(Self::Manual),
            "wake" => Some(Self::Wake),
            "conversation" => Some(Self::Conversation),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Wake => "wake",
            Self::Conversation => "conversation",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ListenOpts {
    pub mode: ListenMode,
    pub wake_word: String,
    pub lang: String,
    pub model: String,
    pub device: Option<String>,
    /// VAD energy threshold override (RMS). 0.0 = unset (env/default wins).
    pub threshold: f32,
}

#[derive(Debug)]
pub enum StartError {
    AlreadyListening,
    NoMicrophone,
    BadMode,
    BadWakeWord,
    BadLang,
    BadDevice,
    BadThreshold,
    ModelFailed(String),
    CaptureFailed(String),
}

impl StartError {
    pub fn http_parts(&self) -> (u16, &'static str, String) {
        match self {
            Self::AlreadyListening => (
                409,
                "already_listening",
                "voice session already active (stop it first)".to_string(),
            ),
            Self::NoMicrophone => (
                503,
                "no_microphone",
                "no microphone found on this machine".to_string(),
            ),
            Self::BadMode => (
                400,
                "invalid_mode",
                "mode must be manual|wake".to_string(),
            ),
            Self::BadWakeWord => (
                400,
                "invalid_wake_word",
                "wake word must be 1-32 characters".to_string(),
            ),
            Self::BadLang => (
                400,
                "invalid_lang",
                "lang must be a 2-letter code (es, en, …)".to_string(),
            ),
            Self::BadDevice => (
                400,
                "invalid_device",
                "unknown input device (see voice status for names)".to_string(),
            ),
            Self::BadThreshold => (
                400,
                "invalid_threshold",
                "threshold must be between 0.005 and 0.1".to_string(),
            ),
            Self::ModelFailed(e) => (502, "model_failed", e.clone()),
            Self::CaptureFailed(e) => (500, "capture_failed", e.clone()),
        }
    }
}

/// Parse + validate the wire options before touching any device.
pub fn parse_opts(
    mode: Option<&str>,
    wake_word: Option<&str>,
    lang: Option<&str>,
    model: Option<&str>,
    device: Option<&str>,
    threshold: Option<f32>,
) -> Result<ListenOpts, StartError> {
    let mode = ListenMode::parse(mode.unwrap_or("manual")).ok_or(StartError::BadMode)?;
    let wake_word = wake_word.unwrap_or("hey").trim().to_string();
    if wake_word.is_empty() || wake_word.chars().count() > 32 {
        return Err(StartError::BadWakeWord);
    }
    let lang = lang.unwrap_or("es").trim().to_lowercase();
    if lang.len() != 2 || !lang.chars().all(|c| c.is_ascii_lowercase()) {
        return Err(StartError::BadLang);
    }
    let device = match device.map(|d| d.trim().to_string()) {
        None => None,
        Some(d) if d.is_empty() => None,
        Some(d) if d.chars().count() <= 128 => Some(d),
        Some(_) => return Err(StartError::BadDevice),
    };
    // 0.0/None = unset (env/default wins); explicit values are clamped to a
    // sane VAD band so a typo can't deafen or hair-trigger the mic.
    let threshold = match threshold {
        None => 0.0,
        Some(t) if (0.005..=0.1).contains(&t) => t,
        Some(_) => return Err(StartError::BadThreshold),
    };
    Ok(ListenOpts {
        mode,
        wake_word,
        lang,
        model: model::model_name_or_default(model),
        device,
        threshold,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VoiceEvent {
    /// Fresh session is live and the mic is open (first event, epoch-tagged).
    Started {},
    Capturing { active: bool },
    Wake { word: String },
    Transcript { text: String },
    Error { code: String, message: String },
    End {},
    /// A2 owned playback position (spec docs/13-a2-tts-engine.md). A global
    /// event, not a listen session: `chunk` is 1-based, `chunks` the
    /// utterance total, `model` the composite catalog id. The poll loop
    /// applies it regardless of epoch; `active:false` ends the turn.
    Speaking {
        active: bool,
        chunk: usize,
        chunks: usize,
        model: String,
    },
}

/// Global speaking-event sink: the TTS engine (`crate::tts`) has no handle
/// on the `VoiceService` (it is built separately in `main`), so the service
/// registers itself here on construction and the engine emits through
/// [`emit_speaking`]. First construction wins; emission is infallible and
/// thread-safe (the queue is mutex-guarded, notify is lock-free).
static SPEAKING_SINK: std::sync::OnceLock<VoiceService> = std::sync::OnceLock::new();

fn register_speaking_sink(service: &VoiceService) {
    let _ = SPEAKING_SINK.set(service.clone());
}

/// Emit a `speaking{active,chunk,chunks,model}` event onto the shared
/// `/v1/voice/events` queue (epoch 0: global, not a listen session).
/// No-op when no voice service exists yet — never blocks, never fails.
pub fn emit_speaking(active: bool, chunk: usize, chunks: usize, model: &str) {
    if let Some(service) = SPEAKING_SINK.get() {
        service.emit_speaking(active, chunk, chunks, model);
    }
}

#[derive(Debug, Clone)]
struct StoredEvent {
    seq: u64,
    epoch: u64,
    event: VoiceEvent,
}

struct SessionHandle {
    stop: Arc<AtomicBool>,
    capturing: Arc<AtomicBool>,
    mode: ListenMode,
    wake_word: String,
    epoch: u64,
}

struct Inner {
    session: Option<SessionHandle>,
    events: VecDeque<StoredEvent>,
    next_seq: u64,
    next_epoch: u64,
}

#[derive(Clone)]
pub struct VoiceService {
    inner: Arc<Mutex<Inner>>,
    notify: Arc<Notify>,
    models_dir: std::path::PathBuf,
}

impl VoiceService {
    pub fn new(models_dir: std::path::PathBuf) -> Self {
        let service = Self {
            inner: Arc::new(Mutex::new(Inner {
                session: None,
                events: VecDeque::with_capacity(64),
                next_seq: 1,
                next_epoch: 1,
            })),
            notify: Arc::new(Notify::new()),
            models_dir,
        };
        // A2 speaking-event sink (the TTS engine emits here globally).
        register_speaking_sink(&service);
        service
    }

    /// Push a `speaking{active,chunk,chunks,model}` event (A2 playback
    /// position) onto this service's queue with epoch 0 (global event).
    pub fn emit_speaking(&self, active: bool, chunk: usize, chunks: usize, model: &str) {
        self.push_event(
            0,
            VoiceEvent::Speaking {
                active,
                chunk,
                chunks,
                model: model.to_string(),
            },
        );
    }

    /// Where whisper model files live (uninstall endpoint needs it).
    pub fn models_dir(&self) -> std::path::PathBuf {
        self.models_dir.clone()
    }

    fn push_event(&self, epoch: u64, event: VoiceEvent) {
        if let Ok(mut inner) = self.inner.lock() {
            let seq = inner.next_seq;
            inner.next_seq += 1;
            inner.events.push_back(StoredEvent { seq, epoch, event });
            while inner.events.len() > 200 {
                inner.events.pop_front();
            }
        }
        self.notify.notify_one();
    }

    /// Snapshot for `GET /v1/voice/status`. Never blocks on audio.
    pub fn status(&self) -> serde_json::Value {
        let (listening, capturing, mode, wake_word) = match self.inner.lock() {
            Ok(inner) => match &inner.session {
                Some(s) => (
                    true,
                    s.capturing.load(Ordering::Relaxed),
                    Some(s.mode.as_str().to_string()),
                    Some(s.wake_word.clone()),
                ),
                None => (false, false, None, None),
            },
            Err(_) => (false, false, None, None),
        };
        let model = model::model_name_or_default(None);
        let models_ready: std::collections::BTreeMap<&str, bool> = model::ALL_MODELS
            .iter()
            .map(|m| (*m, model::model_ready(&self.models_dir, m)))
            .collect();
        serde_json::json!({
            "listening": listening,
            "capturing": capturing,
            "mode": mode,
            "model": model,
            "wake_word": wake_word,
            "mic": audio::microphone_present(),
            "device": audio::input_device_name(),
            "inputs": audio::list_input_devices(),
            "model_ready": model::model_ready(&self.models_dir, &model),
            "models_ready": models_ready,
            "mixer": audio::mixer_state(),
        })
    }

    /// Start listening. Blocks (call from `spawn_blocking`): model download
    /// first, then mic open, then the listener thread owns the rest.
    /// Returns the session epoch: every event carries it, so clients drop
    /// other sessions' stale events instead of acting on them.
    pub fn start(&self, opts: ListenOpts) -> Result<u64, StartError> {
        let stop = Arc::new(AtomicBool::new(false));
        let capturing = Arc::new(AtomicBool::new(false));
        let epoch = {
            let mut inner = self.inner.lock().map_err(|_| {
                StartError::CaptureFailed("voice state poisoned".to_string())
            })?;
            if inner.session.is_some() {
                return Err(StartError::AlreadyListening);
            }
            // Claimed before any blocking work: a second start() fails fast
            // instead of stacking downloads/threads.
            let epoch = inner.next_epoch;
            inner.next_epoch += 1;
            inner.session = Some(SessionHandle {
                stop: stop.clone(),
                capturing: capturing.clone(),
                mode: opts.mode,
                wake_word: opts.wake_word.clone(),
                epoch,
            });
            // Fresh session, fresh queue: a new poller starting at cursor 0
            // must never replay the previous session's Transcript/End (a
            // stale End would instantly kill the new poll loop, orphaning a
            // live session that then 409s every later listen).
            inner.events.clear();
            epoch
        };
        if !audio::microphone_present() {
            self.clear_session();
            return Err(StartError::NoMicrophone);
        }
        // OS gain triage up front: a mic at 33 % looks exactly like a dead
        // mic downstream ("too quiet", best run 0) and no threshold fixes it.
        match audio::mixer_state() {
            Some(m) if m.muted => crate::diagnostics::push(
                "voice: OS mic is MUTED (unmute it, then retry)".to_string(),
            ),
            Some(m) if m.volume < 0.5 => crate::diagnostics::push(format!(
                "voice: OS mic gain is {}% — raise it (wpctl set-volume @DEFAULT_AUDIO_SOURCE@ 1.0) or speech stays under the threshold",
                (m.volume * 100.0).round()
            )),
            _ => {}
        }
        let model_path = match tokio_block_on(model::ensure_downloaded(
            &self.models_dir,
            &opts.model,
        )) {
            Ok(p) => p,
            Err(e) => {
                self.clear_session();
                return Err(StartError::ModelFailed(e));
            }
        };
        let engine = match Engine::load(&model_path) {
            Ok(e) => e,
            Err(e) => {
                self.clear_session();
                return Err(StartError::ModelFailed(e));
            }
        };
        let service = self.clone();
        let thread_stop = stop.clone();
        let thread_capturing = capturing.clone();
        if std::thread::Builder::new()
            .name("smartpc-voice".to_string())
            .spawn(move || {
                run_listener(service, thread_stop, thread_capturing, opts, epoch, engine);
            })
            .is_err()
        {
            self.clear_session();
            return Err(StartError::CaptureFailed("voice thread failed".to_string()));
        }
        // Detached: the thread clears its own record on exit (epoch-guarded,
        // so a slow death can never wipe a newer session).
        Ok(epoch)
    }

    fn clear_session(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.session = None;
        }
    }

    /// Stop listening. Takes the record immediately (recovery always works,
    /// even if the thread already died) and flags the thread; the thread
    /// itself emits the single `End` on exit. Idempotent.
    pub fn stop(&self) {
        let stop = match self.inner.lock() {
            Ok(mut inner) => inner.session.take().map(|s| s.stop),
            Err(_) => None,
        };
        if let Some(flag) = stop {
            flag.store(true, Ordering::Relaxed);
        }
    }

    /// Long-poll: events after `cursor`, waiting up to ~25 s. Returns
    /// `(events, next_cursor)`; empty events + same cursor means "no news".
    pub async fn poll(&self, cursor: u64) -> (Vec<serde_json::Value>, u64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(25);
        loop {
            let drained = match self.inner.lock() {
                Ok(inner) => {
                    let mut out = Vec::new();
                    let mut next = cursor;
                    for stored in inner.events.iter() {
                        if stored.seq > cursor {
                            let mut v = serde_json::to_value(&stored.event)
                                .unwrap_or(serde_json::Value::Null);
                            v["seq"] = serde_json::Value::from(stored.seq);
                            v["epoch"] = serde_json::Value::from(stored.epoch);
                            out.push(v);
                            next = stored.seq;
                        }
                    }
                    Some((out, next.max(cursor)))
                }
                Err(_) => None,
            };
            match drained {
                Some((out, next)) if !out.is_empty() => return (out, next),
                _ => {}
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return (Vec::new(), cursor);
            }
            tokio::select! {
                _ = self.notify.notified() => {},
                _ = tokio::time::sleep(remaining) => {},
            }
        }
    }
}

/// Minimal block_on for the model download inside `start` (which itself runs
/// in `spawn_blocking`, so no runtime exists here).
fn tokio_block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future,
{
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("voice helper runtime")
        .block_on(fut)
}

/// Truncated transcript preview for diagnostics (local log only, the user
/// copies it voluntarily when reporting issues).
fn preview(text: &str) -> String {
    const MAX: usize = 80;
    let flat: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX {
        return format!("'{flat}'");
    }
    let cut: String = flat.chars().take(MAX).collect();
    format!("'{cut}…'")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Arm {
    AwaitingWake,
    AwaitingCommand,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_opts_defaults() {
        let o = parse_opts(None, None, None, None, None, None).unwrap();
        assert_eq!(o.mode, ListenMode::Manual);
        assert_eq!(o.wake_word, "hey");
        assert_eq!(o.lang, "es");
        assert_eq!(o.model, model::DEFAULT_MODEL);
        assert_eq!(o.device, None);
        assert_eq!(o.threshold, 0.0);
    }

    #[test]
    fn parse_opts_rejects_garbage() {
        assert!(matches!(
            parse_opts(Some("shout"), None, None, None, None, None),
            Err(StartError::BadMode)
        ));
        assert!(matches!(
            parse_opts(Some("wake"), Some(""), None, None, None, None),
            Err(StartError::BadWakeWord)
        ));
        assert!(matches!(
            parse_opts(Some("wake"), Some("hey"), Some("espanol"), None, None, None),
            Err(StartError::BadLang)
        ));
        assert!(matches!(
            parse_opts(Some("wake"), Some("hey"), Some("EN"), None, None, None),
            Ok(_)
        ));
        assert!(matches!(
            parse_opts(Some("manual"), None, None, None, Some("Mic"), None),
            Ok(_)
        ));
        assert_eq!(
            parse_opts(Some("manual"), None, None, None, Some("   "), None)
                .unwrap()
                .device,
            None
        );
        assert!(matches!(
            parse_opts(Some("manual"), None, None, None, Some(&"x".repeat(200)), None),
            Err(StartError::BadDevice)
        ));
    }

    #[test]
    fn parse_opts_threshold_band() {
        let o = parse_opts(None, None, None, None, None, Some(0.01)).unwrap();
        assert!((o.threshold - 0.01).abs() < f32::EPSILON);
        assert!(matches!(
            parse_opts(None, None, None, None, None, Some(0.0)),
            Err(StartError::BadThreshold)
        ));
        assert!(matches!(
            parse_opts(None, None, None, None, None, Some(0.5)),
            Err(StartError::BadThreshold)
        ));
        assert!(matches!(
            parse_opts(None, None, None, None, None, Some(-0.01)),
            Err(StartError::BadThreshold)
        ));
        let (code, status, _) = StartError::BadThreshold.http_parts();
        assert_eq!(code, 400);
        assert_eq!(status, "invalid_threshold");
    }

    #[test]
    fn parse_opts_accepts_conversation_mode() {
        let o = parse_opts(Some("conversation"), None, None, None, None, None).unwrap();
        assert_eq!(o.mode, ListenMode::Conversation);
        assert_eq!(ListenMode::Conversation.as_str(), "conversation");
    }

    #[test]
    fn stop_without_session_is_a_noop() {
        let svc = VoiceService::new(std::env::temp_dir());
        svc.stop();
        svc.stop();
        let st = svc.status();
        assert_eq!(st["listening"], false);
    }

    #[test]
    fn already_listening_error_shape() {
        let (code, status, _) = StartError::AlreadyListening.http_parts();
        assert_eq!(code, 409);
        assert_eq!(status, "already_listening");
    }

    #[test]
    fn speaking_event_serializes_to_the_a2_shape() {
        // Contract with the renderer poll loop: tagged `speaking` with
        // {active, chunk, chunks, model}; the poll envelope adds seq/epoch.
        let v = serde_json::to_value(&VoiceEvent::Speaking {
            active: true,
            chunk: 2,
            chunks: 3,
            model: "upstream-piper-es-sharvard#0".to_string(),
        })
        .unwrap();
        assert_eq!(v["type"], "speaking");
        assert_eq!(v["active"], true);
        assert_eq!(v["chunk"], 2);
        assert_eq!(v["chunks"], 3);
        assert_eq!(v["model"], "upstream-piper-es-sharvard#0");
    }

    #[test]
    fn preview_truncates_long_transcripts() {
        assert_eq!(super::preview("hola mundo"), "'hola mundo'");
        assert_eq!(super::preview("  a  b  "), "'a b'");
        let long = "word ".repeat(30);
        let p = super::preview(&long);
        assert!(p.ends_with("…'"));
        assert!(p.chars().count() <= 84);
    }
}

fn run_listener(
    service: VoiceService,
    stop: Arc<AtomicBool>,
    capturing: Arc<AtomicBool>,
    opts: ListenOpts,
    epoch: u64,
    engine: Engine,
) {
    // A panicking listener must never poison future sessions: catch it,
    // report it, release the record (epoch-guarded like every other exit).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Listener::new(service.clone(), stop, capturing, opts, epoch, engine).run();
    }));
    if let Err(_) = result {
        service.push_event(
            epoch,
            VoiceEvent::Error {
                code: "voice_crash".to_string(),
                message: "voice worker failed unexpectedly".to_string(),
            },
        );
        crate::diagnostics::push("voice: listener thread panicked".to_string());
    }
    service.push_event(epoch, VoiceEvent::End {});
    if let Ok(mut inner) = service.inner.lock() {
        let ours = inner
            .session
            .as_ref()
            .is_some_and(|s| s.epoch == epoch);
        if ours {
            inner.session = None;
        }
    }
    service.notify.notify_one();
}

struct Listener {
    service: VoiceService,
    stop: Arc<AtomicBool>,
    capturing_flag: Arc<AtomicBool>,
    opts: ListenOpts,
    epoch: u64,
    engine: Engine,
    vad: Vad,
    preroll: VecDeque<f32>,
    utter: Vec<f32>,
    capturing: bool,
    arm: Arm,
    max_utter_samples: usize,
    // Observability for "nothing arrives" reports: frame flow + level floor.
    frames_seen: u64,
    max_rms: f32,
    captures: u64,
    /// Consecutive empty transcriptions (noise). Bounds the hot-mic loop:
    /// a sensitive threshold on room noise would otherwise transcribe
    /// forever and never deliver text.
    empty_runs: u32,
}

/// Empty transcriptions in a row before a manual/conversation turn gives
/// up (wake mode is exempt: indefinite arming is its job).
const MAX_EMPTY_RUNS: u32 = 5;

impl Listener {
    fn new(
        service: VoiceService,
        stop: Arc<AtomicBool>,
        capturing_flag: Arc<AtomicBool>,
        opts: ListenOpts,
        epoch: u64,
        engine: Engine,
    ) -> Self {
        let max_s: u64 = std::env::var("VOICE_MAX_UTTERANCE_S")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(30);
        // Per-call threshold wins over the env/default (the UI sensitivity
        // control rides here); 0.0 means "not set".
        let mut vad_cfg = VadConfig::from_env();
        if opts.threshold > 0.0 {
            vad_cfg.threshold = opts.threshold;
        }
        Self {
            service,
            stop,
            capturing_flag,
            opts,
            epoch,
            engine,
            vad: Vad::new(vad_cfg),
            preroll: VecDeque::with_capacity(FRAME_SAMPLES * 10),
            utter: Vec::with_capacity(TARGET_RATE as usize * 8),
            capturing: false,
            arm: Arm::AwaitingWake,
            max_utter_samples: (max_s.max(5) as usize) * TARGET_RATE as usize,
            frames_seen: 0,
            max_rms: 0.0,
            captures: 0,
            empty_runs: 0,
        }
    }

    fn emit(&self, event: VoiceEvent) {
        self.service.push_event(self.epoch, event);
    }

    fn set_capturing(&mut self, on: bool) {
        self.capturing = on;
        self.capturing_flag.store(on, Ordering::Relaxed);
        self.emit(VoiceEvent::Capturing { active: on });
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn run(mut self) {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Result<Vec<f32>, String>>(128);
        let (_stream, desc) = match audio::open_capture(
            tx.clone(),
            self.opts.device.as_deref(),
        ) {
            Ok(s) => s,
            Err(e) => {
                let (code, message) = match &e {
                    audio::CaptureError::NoMicrophone => (
                        "no_microphone",
                        "no microphone found on this machine",
                    ),
                    audio::CaptureError::Unsupported(d) => ("unsupported_audio", d.as_str()),
                    audio::CaptureError::Stream(d) => ("capture_failed", d.as_str()),
                };
                self.emit(VoiceEvent::Error {
                    code: code.to_string(),
                    message: message.to_string(),
                });
                // The tail emits End + releases the record (epoch-guarded).
                return;
            }
        };
        crate::diagnostics::push(format!(
            "voice: listening (mode={}, lang={}, dev={}, {}ch@{}Hz {:?}, vad thr={} sil_ms={} min_ms={} gap_ms={})",
            self.opts.mode.as_str(),
            self.opts.lang,
            desc.device,
            desc.channels,
            desc.rate,
            desc.format,
            self.vad.threshold(),
            self.vad.silence_ms(),
            self.vad.min_speech_ms(),
            self.vad.gap_ms(),
        ));
        self.emit(VoiceEvent::Started {});
        while !self.stopped() {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(Ok(frame)) => self.on_frame(&frame),
                Ok(Err(e)) => {
                    self.emit(VoiceEvent::Error {
                        code: "capture_failed".to_string(),
                        message: e,
                    });
                    break;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        // Session summary: distinguishes "mic silent/wrong" (frames flowed,
        // nothing captured) from "stream stalled" (no frames at all). The
        // best run tells "too quiet" (0–2 hits) from "hovering at the gate"
        // (near the hit count): the former wants gain/sensitivity, the
        // latter used to be the consecutive-frames reset.
        if self.captures == 0 && self.frames_seen > 0 {
            crate::diagnostics::push(format!(
                "voice: session ended, no speech captured ({} frames, max rms {:.4}, best run {}/{})",
                self.frames_seen,
                self.max_rms,
                self.vad.best_run(),
                self.vad.min_speech_hits(),
            ));
        } else if self.frames_seen == 0 {
            crate::diagnostics::push(
                "voice: session ended, NO AUDIO FRAMES arrived (stream stalled?)".to_string(),
            );
        }
        // Single End for every exit path lives in run_listener's tail.
    }

    fn on_frame(&mut self, frame: &[f32]) {
        // Level tracing: the first frame proves audio flows at all; the
        // periodic floor shows whether the mic hears the room or digital
        // silence (wrong device / muted at OS level).
        let rms = audio::rms(frame);
        self.frames_seen += 1;
        self.max_rms = self.max_rms.max(rms);
        if self.frames_seen == 1 {
            crate::diagnostics::push(format!(
                "voice: audio flowing, first frame rms={rms:.4}"
            ));
        } else if !self.capturing && self.frames_seen % 500 == 0 {
            crate::diagnostics::push(format!(
                "voice: idle level rms={:.4} (max {:.4} over {} frames, no speech)",
                rms, self.max_rms, self.frames_seen
            ));
            self.max_rms = 0.0;
        }
        // Pre-roll always runs so SpeechStart loses nothing (~300 ms ring).
        if !self.capturing {
            self.preroll.extend(frame.iter().copied());
            while self.preroll.len() > FRAME_SAMPLES * 10 {
                self.preroll.pop_front();
            }
        }
        match self.vad.feed(rms) {
            VadTransition::Silence => {}
            VadTransition::SpeechStart => {
                self.utter.clear();
                self.utter.extend(self.preroll.drain(..));
                self.utter.extend_from_slice(frame);
                self.set_capturing(true);
                self.captures += 1;
                crate::diagnostics::push("voice: speech detected, capturing…".to_string());
            }
            VadTransition::SpeechOngoing => {
                if self.capturing {
                    self.utter.extend_from_slice(frame);
                    if self.utter.len() >= self.max_utter_samples {
                        self.finalize();
                    }
                }
            }
            VadTransition::SpeechEnd => {
                if self.capturing {
                    self.utter.extend_from_slice(frame);
                    self.finalize();
                }
            }
        }
    }

    fn finish_capture(&mut self) -> Vec<f32> {
        self.set_capturing(false);
        std::mem::take(&mut self.utter)
    }

    fn finalize(&mut self) {
        let pcm = self.finish_capture();
        if self.stopped() {
            return;
        }
        // Stage tracing (all local, user-copied): each line narrows a
        // "nothing arrives" report to VAD vs transcription vs matching.
        crate::diagnostics::push(format!(
            "voice: transcribing {} samples…",
            pcm.len()
        ));
        let text = match self.engine.transcribe(&pcm, &self.opts.lang) {
            Ok(t) => t,
            Err(e) => {
                crate::diagnostics::push(format!("voice: transcription failed: {e}"));
                self.emit(VoiceEvent::Error {
                    code: "transcribe_failed".to_string(),
                    message: e,
                });
                // Terminal: a broken engine won't heal mid-session. The tail
                // emits End and releases the record so the next press works.
                self.stop.store(true, Ordering::Relaxed);
                return;
            }
        };
        if text.is_empty() {
            // Breath, chair, fan: the VAD fired on nothing linguistic.
            crate::diagnostics::push(
                "voice: transcript empty (noise, not speech?)".to_string(),
            );
            // Hot mic + sensitive threshold = endless noise loop (transcribe
            // forever, deliver nothing). Manual/conversation turns give up
            // after a bounded run; wake mode is exempt (arming is its job).
            self.empty_runs += 1;
            if self.empty_runs >= MAX_EMPTY_RUNS
                && matches!(
                    self.opts.mode,
                    ListenMode::Manual | ListenMode::Conversation
                )
            {
                crate::diagnostics::push(
                    "voice: 5 empty transcriptions in a row, ending (threshold likely too sensitive)".to_string(),
                );
                self.stop.store(true, Ordering::Relaxed);
            }
            return;
        }
        self.empty_runs = 0;
        crate::diagnostics::push(format!("voice: heard {}", preview(&text)));
        match self.opts.mode {
            // Conversation ends the backend turn exactly like manual; the
            // renderer owns the loop (next listen after the spoken reply).
            ListenMode::Manual | ListenMode::Conversation => {
                self.emit(VoiceEvent::Transcript { text });
                // Single utterance per press: the frontend sends it and the
                // session is done. Nobody presses anything to finish.
                self.stop.store(true, Ordering::Relaxed);
            }
            ListenMode::Wake => match self.arm {
                Arm::AwaitingWake => {
                    match split_wake_command(&text, &self.opts.wake_word) {
                        None => {
                            // Background chatter, re-arm silently (but trace:
                            // a perpetually-missing wake word is the #1
                            // support question).
                            crate::diagnostics::push(format!(
                                "voice: no wake word in onset, re-arming ({})",
                                preview(&text)
                            ));
                        }
                        Some(remainder) => {
                            crate::diagnostics::push(format!(
                                "voice: wake word heard ({})",
                                self.opts.wake_word
                            ));
                            self.emit(VoiceEvent::Wake {
                                word: self.opts.wake_word.clone(),
                            });
                            if remainder.trim().is_empty() {
                                // Bare "hey" (pause): the command comes next.
                                self.arm = Arm::AwaitingCommand;
                            } else {
                                // One breath ("hey, do X"): send it now and
                                // stay armed for the next one.
                                self.emit(VoiceEvent::Transcript {
                                    text: remainder,
                                });
                            }
                        }
                    }
                }
                Arm::AwaitingCommand => {
                    self.emit(VoiceEvent::Transcript { text });
                    self.arm = Arm::AwaitingWake;
                }
            },
        }
    }
}
