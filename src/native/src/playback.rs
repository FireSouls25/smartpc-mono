//! A2 owned playback: cpal-direct WAV queue with REAL completion.
//!
//! Why cpal-direct (spec deviation — rodio 0.20 cannot link on Linux:
//! its cpal 0.15 pulls alsa 0.9 while our cpal 0.18 pulls alsa 0.11 and
//! cargo allows only one `links="alsa"` crate per graph): the synth wavs
//! are our own 16-bit PCM mono files, so decode is ~40 lines, resampling
//! is linear (voice-grade, same as the capture path), and no new crates
//! or system libs are needed — the purest form of the portability goal.
//!
//! Shape: the blocking playback task owns the cpal `Stream` end-to-end
//! (it is created, played, and dropped on one thread); async code only
//! shares the `PlayHandle` (stop flag + finished flag). Completion is
//! observed, never guessed: the data callback counts consumed device
//! frames, and per-chunk `speaking` events fire as playback crosses each
//! chunk boundary. The estimate watchdog is a backup that LOGS ONLY — it
//! never reaps live audio.
//!
//! Headless tests stay device-free: WAV decode, resampling, chunking,
//! and the event series are pure; live stream creation is gated behind
//! `SMARTPC_AUDIO_LIVE=1`.

use std::io::BufReader;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Synth-chunk ceiling per daemon call (mirrors the client's 400-char
/// pump chunks; the daemon itself caps at 4000).
pub const SYNTH_CHUNK_LEN: usize = 400;

/// How often the playback task polls the consumed-frame counter while
/// audio plays. Short enough for exact highlight advance.
const POLL_MS: u64 = 50;

/// No-progress backstop: a stream that stops delivering callbacks (or
/// errors silently) never parks the playback task forever.
const STALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Sentence-aware split of already-cleaned text into synthable chunks.
/// Newlines are hard breaks; overlong sentences word-wrap (hard cut when
/// no space is found). Empty input yields no chunks.
pub fn split_chunks(text: &str) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    // `\n+` runs are hard breaks; other runs end at a sentence terminator.
    let mut rest = text.trim();
    let mut parts: Vec<&str> = Vec::new();
    while !rest.is_empty() {
        if rest.starts_with('\n') {
            let n = rest.chars().take_while(|c| *c == '\n').count();
            parts.push(&rest[..n]);
            rest = rest[n..].trim_start_matches('\n');
            rest = rest.trim_start();
            continue;
        }
        let mut end: Option<usize> = None;
        for (i, c) in rest.char_indices() {
            if matches!(c, '.' | '!' | '?' | '…') {
                end = Some(i + c.len_utf8());
                break;
            }
        }
        match end {
            Some(e) => {
                // Swallow one run of closing quotes/spaces after the mark.
                let mut j = e;
                for (k, c) in rest[e..].char_indices() {
                    if matches!(c, '"' | '»' | '”' | '\'' | '’' | ' ' | '\t') {
                        j = e + k + c.len_utf8();
                    } else {
                        break;
                    }
                }
                parts.push(rest[..j].trim());
                // Spaces/tabs only: a following newline run must survive
                // so the loop below sees it as a hard break.
                rest = rest[j..].trim_start_matches([' ', '\t']);
            }
            None => {
                parts.push(rest.trim());
                break;
            }
        }
    }
    let mut push = |s: &str| {
        let t = s.trim();
        if !t.is_empty() {
            chunks.push(t.to_string());
        }
    };
    for part in parts {
        let mut piece = part.trim();
        if piece.is_empty() {
            // Hard break (a newline run): flush the pending chunk so the
            // break survives packing.
            if !current.is_empty() {
                push(&current);
                current.clear();
            }
            continue;
        }
        while piece.chars().count() > SYNTH_CHUNK_LEN {
            if !current.is_empty() {
                push(&current);
                current.clear();
            }
            // Last space within the window (byte index); fall back to a
            // hard cut after exactly SYNTH_CHUNK_LEN chars otherwise.
            let mut cut: Option<usize> = None;
            let mut hard = piece.len();
            for (n, (i, c)) in piece.char_indices().enumerate() {
                if n == SYNTH_CHUNK_LEN {
                    hard = i;
                    break;
                }
                if c == ' ' {
                    cut = Some(i);
                }
            }
            let cut = cut.unwrap_or(hard).max(1);
            let cut = cut.max(1);
            push(&piece[..cut]);
            piece = piece[cut..].trim();
        }
        if piece.is_empty() {
            continue;
        }
        let candidate = if current.is_empty() {
            piece.to_string()
        } else {
            format!("{current} {piece}")
        };
        if candidate.chars().count() <= SYNTH_CHUNK_LEN {
            current = candidate;
        } else {
            push(&current);
            current = piece.to_string();
        }
    }
    push(&current);
    chunks
}

/// One point of the speaking-event series for a `chunks`-chunk utterance:
/// per-chunk `active` in 1-based order, then a final `inactive` echoing
/// the last position (the client resets its highlight on `active:false`).
/// The playback loop below follows this exact series. It is only
/// constructed in tests (headless queue-math/event-sequence coverage —
/// the normative gate that never opens an audio device).
// (allow: binary-crate lint; the test module is the real user.)
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpeakingPoint {
    pub active: bool,
    pub chunk: usize,
    pub chunks: usize,
}

// (allow: see SpeakingPoint — headless spec surface for tests.)
#[allow(dead_code)]
pub fn speaking_sequence(chunks: usize) -> Vec<SpeakingPoint> {
    let mut out = Vec::with_capacity(chunks + 1);
    for i in 1..=chunks {
        out.push(SpeakingPoint {
            active: true,
            chunk: i,
            chunks,
        });
    }
    out.push(SpeakingPoint {
        active: false,
        chunk: chunks,
        chunks,
    });
    out
}

// ─── WAV decode (16-bit PCM mono — the only shape our synth writes) ─────────

/// Decoded synth chunk: mono i16 samples at their native rate.
pub struct DecodedWav {
    pub samples: Vec<i16>,
    pub rate: u32,
}

fn u16_le(b: &[u8]) -> u16 {
    u16::from_le_bytes([b[0], b[1]])
}

fn u32_le(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Parse a 16-bit PCM mono WAV (chunk-walking, so extra LIST/fact chunks
/// are skipped). Fails closed on anything else — synth files we didn't
/// write never reach the speaker.
pub fn decode_wav_16(path: &PathBuf) -> Result<DecodedWav, String> {
    use std::io::Read as _;
    let mut bytes = Vec::new();
    BufReader::new(
        std::fs::File::open(path).map_err(|e| format!("unreadable wav: {e}"))?,
    )
    .read_to_end(&mut bytes)
    .map_err(|e| format!("unreadable wav: {e}"))?;
    decode_wav_16_bytes(&bytes).map_err(|e| format!("{}: {e}", path.display()))
}

fn decode_wav_16_bytes(bytes: &[u8]) -> Result<DecodedWav, String> {
    if bytes.len() < 44
        || &bytes[0..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
    {
        return Err("not a WAV file".to_string());
    }
    let mut fmt_rate = 0u32;
    let mut fmt_ok = false;
    let mut data: &[u8] = &[];
    let mut pos = 12;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32_le(&bytes[pos + 4..pos + 8]) as usize;
        let body = &bytes[pos + 8..(pos + 8 + len).min(bytes.len())];
        if id == b"fmt " {
            if body.len() < 16 {
                return Err("truncated fmt chunk".to_string());
            }
            let audio_format = u16_le(&body[0..2]);
            let channels = u16_le(&body[2..4]);
            let bits = u16_le(&body[14..16]);
            if audio_format != 1 || channels != 1 || bits != 16 {
                return Err(format!(
                    "unsupported wav (need 16-bit PCM mono, got format={audio_format} ch={channels} bits={bits})"
                ));
            }
            fmt_rate = u32_le(&body[4..8]);
            if fmt_rate == 0 {
                return Err("wav with zero sample rate".to_string());
            }
            fmt_ok = true;
        } else if id == b"data" {
            data = body;
        }
        pos += 8 + len + (len % 2); // chunks are word-padded
    }
    if !fmt_ok {
        return Err("wav without fmt chunk".to_string());
    }
    if data.is_empty() {
        return Err("wav without audio".to_string());
    }
    let samples = data
        .chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(DecodedWav {
        samples,
        rate: fmt_rate,
    })
}

/// Linear-resample mono i16 to interleaved device-rate f32 (channel
/// duplicated). Voice-grade, same approach as the capture path — avoids a
/// DSP crate for one call site.
pub fn resample_to_device(
    samples: &[i16],
    from_rate: u32,
    to_rate: u32,
    channels: usize,
) -> Vec<f32> {
    if samples.is_empty() || channels == 0 {
        return Vec::new();
    }
    let mono: Vec<f32> = if from_rate == to_rate {
        samples.iter().map(|s| *s as f32 / 32768.0).collect()
    } else {
        let ratio = from_rate as f64 / to_rate as f64;
        let out_len = ((samples.len() as f64) / ratio).ceil() as usize;
        (0..out_len)
            .map(|i| {
                let pos = i as f64 * ratio;
                let i0 = pos.floor() as usize;
                let frac = (pos - i0 as f64) as f32;
                let s0 = samples.get(i0).copied().unwrap_or(0) as f32 / 32768.0;
                let s1 = samples.get(i0 + 1).copied().unwrap_or_else(|| {
                    samples.get(i0).copied().unwrap_or(0)
                }) as f32
                    / 32768.0;
                s0 + (s1 - s0) * frac
            })
            .collect()
    };
    let mut out = Vec::with_capacity(mono.len() * channels);
    for s in mono {
        for _ in 0..channels {
            out.push(s);
        }
    }
    out
}

// ─── Playback ────────────────────────────────────────────────────────────────

/// Shared live-playback handle: the blocking task owns the cpal `Stream`
/// (created, played, and dropped on one thread); everyone else only sees
/// this. `stop` halts audio within one device buffer (the callback emits
/// silence) and unblocks the playback poll; `finished` marks natural end
/// (the watchdog's "still playing?" check).
#[derive(Debug, Clone)]
pub struct PlayHandle {
    pub stop: Arc<AtomicBool>,
    pub finished: Arc<AtomicBool>,
}

impl PlayHandle {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            finished: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Slot for the current utterance's handle: `None` = nothing is (or should
/// be) playing. A finished utterance clears only its own handle (a newer
/// turn may have stored another already).
#[derive(Debug, Clone, Default)]
pub struct LiveSink(Arc<Mutex<Option<Arc<PlayHandle>>>>);

impl LiveSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, handle: Arc<PlayHandle>) {
        if let Ok(mut slot) = self.0.lock() {
            *slot = Some(handle);
        }
    }

    pub fn get(&self) -> Option<Arc<PlayHandle>> {
        self.0.lock().ok().and_then(|slot| slot.clone())
    }

    /// Cut audio now. Idempotent; never touches the synth daemon.
    pub fn stop(&self) {
        if let Ok(mut slot) = self.0.lock() {
            if let Some(handle) = slot.take() {
                handle.stop.store(true, Ordering::Relaxed);
            }
        }
    }

    /// Forget the slot only when it still holds our handle (a newer
    /// utterance may have stored its own already — never drop another
    /// turn's stop handle).
    pub fn clear_if_same(&self, handle: &Arc<PlayHandle>) {
        if let Ok(mut slot) = self.0.lock() {
            if slot.as_ref().is_some_and(|h| Arc::ptr_eq(h, handle)) {
                *slot = None;
            }
        }
    }

    pub fn playing(&self) -> bool {
        self.get()
            .is_some_and(|h| !h.finished.load(Ordering::Relaxed))
    }
}

/// Device output description (logged: rate surprises are audible).
#[derive(Debug, Clone)]
pub struct OutputDesc {
    pub rate: u32,
    pub channels: usize,
    pub format: String,
}

/// Open the default output for one utterance: decode + resample every wav
/// to a single gapless device-rate buffer, build the stream, and hand back
/// everything the playback poll needs. Device-absent (or undecodable —
/// when nothing playable remains) fails closed with an honest error so
/// `speak` never goes silently mute.
pub struct OpenedOutput {
    pub stream: cpal::Stream,
    pub desc: OutputDesc,
    /// Cumulative device-FRAME boundaries: `boundaries[i]` = frames of
    /// chunks `0..=i`. Crossing `boundaries[i]` means chunk `i` is done.
    pub boundaries: Vec<usize>,
    pub total_frames: usize,
    pub consumed: Arc<AtomicUsize>,
    pub stream_error: Arc<Mutex<Option<String>>>,
}

pub fn open_output(wavs: &[PathBuf], stop: &Arc<AtomicBool>) -> Result<OpenedOutput, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no audio device".to_string())?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("no audio device: {e}"))?;
    let rate = supported.sample_rate();
    let channels = supported.channels() as usize;
    if channels == 0 {
        return Err("no audio device: 0 channels".to_string());
    }
    let format = supported.sample_format();
    let desc = OutputDesc {
        rate,
        channels,
        format: format!("{format:?}"),
    };
    // Decode + resample upfront (a few MB for a capped utterance): one
    // gapless buffer, chunk boundaries in device frames for exact events.
    let mut buffer: Vec<f32> = Vec::new();
    let mut boundaries: Vec<usize> = Vec::new();
    for wav in wavs {
        match decode_wav_16(wav) {
            Ok(decoded) => {
                buffer.extend(resample_to_device(
                    &decoded.samples,
                    decoded.rate,
                    rate,
                    channels,
                ));
                boundaries.push(buffer.len() / channels);
            }
            Err(e) => crate::diagnostics::push(format!("tts: skipping chunk ({e})")),
        }
    }
    if buffer.is_empty() {
        return Err("no playable audio".to_string());
    }
    let total_frames = buffer.len() / channels;
    let consumed = Arc::new(AtomicUsize::new(0));
    let stream_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stream = build_stream(
        &device,
        StreamParams {
            config: supported.config(),
            format,
            channels,
            buffer,
            consumed: consumed.clone(),
            stop: stop.clone(),
            stream_error: stream_error.clone(),
        },
    )?;
    Ok(OpenedOutput {
        stream,
        desc,
        boundaries,
        total_frames,
        consumed,
        stream_error,
    })
}

/// Everything `build_stream` needs beyond the device itself.
struct StreamParams {
    config: cpal::StreamConfig,
    format: cpal::SampleFormat,
    channels: usize,
    buffer: Vec<f32>,
    consumed: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    stream_error: Arc<Mutex<Option<String>>>,
}

fn build_stream(
    device: &cpal::Device,
    params: StreamParams,
) -> Result<cpal::Stream, String> {
    let StreamParams {
        config,
        format,
        channels,
        buffer,
        consumed,
        stop,
        stream_error,
    } = params;
    let err_slot = stream_error;
    let err_fn = move |err| {
        if let Ok(mut slot) = err_slot.lock() {
            *slot = Some(format!("{err}"));
        }
    };
    match format {
        cpal::SampleFormat::F32 => {
            let feed = Feed::new(buffer, channels, &consumed, &stop);
            device
                .build_output_stream(
                    config,
                    move |data: &mut [f32], _| feed.fill_f32(data),
                    err_fn,
                    None,
                )
                .map_err(|e| format!("audio stream failed: {e}"))
        }
        cpal::SampleFormat::I16 => {
            let feed = Feed::new(buffer, channels, &consumed, &stop);
            device
                .build_output_stream(
                    config,
                    move |data: &mut [i16], _| feed.fill_i16(data),
                    err_fn,
                    None,
                )
                .map_err(|e| format!("audio stream failed: {e}"))
        }
        cpal::SampleFormat::U16 => {
            let feed = Feed::new(buffer, channels, &consumed, &stop);
            device
                .build_output_stream(
                    config,
                    move |data: &mut [u16], _| feed.fill_u16(data),
                    err_fn,
                    None,
                )
                .map_err(|e| format!("audio stream failed: {e}"))
        }
        f => Err(format!("unsupported audio format: {f:?}")),
    }
}

/// Gapless feeder: one device-rate interleaved buffer shared with the
/// audio thread; `consumed` counts played FRAMES (the completion
/// signal). `stop` flips output to silence immediately (cut within one
/// device buffer); the task then drops the stream. Callbacks never block:
/// pure array walks, no locks, no allocation.
struct Feed {
    buffer: Vec<f32>,
    channels: usize,
    consumed: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Feed {
    fn new(
        buffer: Vec<f32>,
        channels: usize,
        consumed: &Arc<AtomicUsize>,
        stop: &Arc<AtomicBool>,
    ) -> Self {
        Self {
            buffer,
            channels: channels.max(1),
            consumed: consumed.clone(),
            stop: stop.clone(),
        }
    }

    fn frames_total(&self) -> usize {
        self.buffer.len() / self.channels
    }

    /// Fill one callback of interleaved samples, advancing the frame
    /// counter by the frames actually served (saturates at the end;
    /// cpal delivers whole frames per callback, so frame math is exact).
    fn fill<T>(&self, data: &mut [T], conv: impl Fn(f32) -> T)
    where
        T: Clone,
    {
        let stopped = self.stop.load(Ordering::Relaxed);
        let pos = self.consumed.load(Ordering::Relaxed);
        let total = self.frames_total();
        let frames = data.len() / self.channels;
        let served = frames.min(total.saturating_sub(pos));
        for f in 0..frames {
            // The buffer duplicates mono across channels: every channel of
            // a frame carries the same value — index the first lane.
            let v = if stopped || f >= served {
                0.0
            } else {
                self.buffer[(pos + f) * self.channels]
            };
            let out = conv(v);
            for c in 0..self.channels {
                data[f * self.channels + c] = out.clone();
            }
        }
        self.consumed.fetch_add(served, Ordering::Relaxed);
    }

    fn fill_f32(&self, data: &mut [f32]) {
        self.fill(data, |v| v);
    }

    fn fill_i16(&self, data: &mut [i16]) {
        self.fill(data, |v: f32| {
            (v.clamp(-1.0, 1.0) * 32767.0).round() as i16
        });
    }

    fn fill_u16(&self, data: &mut [u16]) {
        self.fill(data, |v: f32| {
            ((v.clamp(-1.0, 1.0) + 1.0) * 32767.5).round() as u16
        });
    }
}

/// Generation guard: true while `generation` is still the live turn.
pub type IsCurrent = Arc<dyn Fn(u64) -> bool + Send + Sync>;
/// Last-position memory for `speak-stop` (chunk, chunks, model).
pub type RememberPos = Arc<dyn Fn(usize, usize, &str) + Send + Sync>;

/// A queued utterance: everything the blocking playback task needs.
/// `is_current` / `remember` close over the manager state (generation
/// guard + last-position memory for `speak-stop`).
pub struct QueueJob {
    pub live: LiveSink,
    pub files: Vec<PathBuf>,
    pub model: String,
    pub generation: u64,
    pub stop: Arc<AtomicBool>,
    pub is_current: IsCurrent,
    pub remember: RememberPos,
}

/// Play a queued utterance to REAL per-chunk completion on the blocking
/// task (it owns the cpal `Stream` end-to-end). Series per
/// [`speaking_sequence`]: `active` per chunk in 1-based order as playback
/// crosses each chunk boundary, then one `inactive` — but only while still
/// current. A superseded job stands down silently (the winning turn
/// already owns the event series). `started` is the speak handshake:
/// device-open success or the honest error, so `speak` fails loudly
/// instead of going silently mute.
pub fn run_queue_blocking(
    job: QueueJob,
    started: tokio::sync::oneshot::Sender<Result<(), String>>,
) {
    let current = || (job.is_current)(job.generation);
    let opened = match open_output(&job.files, &job.stop) {
        Ok(o) => o,
        Err(e) => {
            let _ = started.send(Err(e));
            return;
        }
    };
    if let Err(e) = opened.stream.play() {
        let _ = started.send(Err(format!("audio stream failed: {e}")));
        return;
    }
    crate::diagnostics::push(format!(
        "tts: playing {} chunk(s) ({}Hz {}ch {:?})",
        job.files.len(),
        opened.desc.rate,
        opened.desc.channels,
        opened.desc.format,
    ));
    if !current() {
        // Cut between spawn and open: the stop owns the turn already.
        let _ = started.send(Ok(()));
        return;
    }
    let handle = Arc::new(PlayHandle::new(job.stop.clone()));
    job.live.set(handle.clone());
    let _ = started.send(Ok(()));
    let total_chunks = opened.boundaries.len();
    let mut emitted = 0usize;
    // Chunk 1 starts now.
    if total_chunks > 0 {
        (job.remember)(1, total_chunks, &job.model);
        crate::stt::session::emit_speaking(true, 1, total_chunks, &job.model);
        emitted = 1;
    }
    let mut last_progress = Instant::now();
    let mut last_pos = 0usize;
    loop {
        if job.stop.load(Ordering::Relaxed) || !current() {
            return; // cut or superseded: the stop already owns the series
        }
        if let Ok(slot) = opened.stream_error.lock() {
            if let Some(msg) = slot.as_ref() {
                crate::diagnostics::push(format!("tts: audio stream error: {msg}"));
                if current() {
                    crate::stt::session::emit_speaking(
                        false,
                        emitted,
                        total_chunks,
                        &job.model,
                    );
                }
                return;
            }
        }
        let pos = opened.consumed.load(Ordering::Relaxed);
        if pos != last_pos {
            last_pos = pos;
            last_progress = Instant::now();
        }
        while emitted < total_chunks && pos >= opened.boundaries[emitted] {
            emitted += 1;
            if emitted < total_chunks {
                (job.remember)(emitted + 1, total_chunks, &job.model);
                if current() {
                    crate::stt::session::emit_speaking(
                        true,
                        emitted + 1,
                        total_chunks,
                        &job.model,
                    );
                }
            }
        }
        if pos >= opened.total_frames {
            break;
        }
        if last_progress.elapsed() > STALL_TIMEOUT {
            crate::diagnostics::push(
                "tts: audio stall (no progress), ending turn".to_string(),
            );
            if current() {
                crate::stt::session::emit_speaking(false, emitted, total_chunks, &job.model);
            }
            return;
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
    if current() {
        handle.finished.store(true, Ordering::Relaxed);
        crate::stt::session::emit_speaking(false, total_chunks, total_chunks, &job.model);
        job.live.clear_if_same(&handle);
    }
}

/// Backup watchdog: LOGS ONLY when playback is still running past the
/// estimate. It never stops, clears, or otherwise touches live audio —
/// completion events are the authority, this is the paper trail.
pub fn spawn_watchdog(
    handle: Arc<PlayHandle>,
    generation: u64,
    estimated_ms: u64,
    is_current: IsCurrent,
) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(estimated_ms)).await;
        if is_current(generation) && !handle.finished.load(Ordering::Relaxed) {
            crate::diagnostics::push(
                "tts: watchdog: still playing past estimate (observing only, live audio untouched)"
                    .to_string(),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal 16-bit mono WAV writer (mirrors the daemon's `writeWav16`).
    fn write_test_wav(path: &PathBuf, samples: &[i16], rate: u32) {
        let mut out = Vec::with_capacity(44 + samples.len() * 2);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + samples.len() as u32 * 2).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * 2).to_le_bytes());
        out.extend_from_slice(&2u16.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(samples.len() as u32 * 2).to_le_bytes());
        for s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(path, &out).unwrap();
    }

    #[test]
    fn split_empty_yields_no_chunks() {
        assert!(split_chunks("").is_empty());
        assert!(split_chunks("   \n  ").is_empty());
    }

    #[test]
    fn split_short_text_is_one_chunk() {
        assert_eq!(split_chunks("hola mundo"), vec!["hola mundo"]);
    }

    #[test]
    fn split_packs_short_sentences_and_breaks_newlines() {
        // Short sentences pack into one synth chunk (fewer daemon calls).
        assert_eq!(
            split_chunks("Primera frase. Segunda frase!"),
            vec!["Primera frase. Segunda frase!"]
        );
        // Newlines are hard breaks even when packing would fit.
        assert_eq!(
            split_chunks("Primera frase.\nTercera línea"),
            vec!["Primera frase.", "Tercera línea"]
        );
        // Past the ceiling, sentences spill into their own chunks.
        let big = format!("{} Fin.", "lorem ipsum dolor ".repeat(30));
        let chunks = split_chunks(&big);
        assert!(chunks.len() >= 2);
        assert!(chunks.last().unwrap().ends_with("Fin."));
    }

    #[test]
    fn split_packs_short_sentences_and_wraps_long_ones() {
        let mut long = String::from("palabra ");
        while long.chars().count() < SYNTH_CHUNK_LEN + 50 {
            long.push_str("palabra ");
        }
        let chunks = split_chunks(&format!("Corta. {long}Fin."));
        assert!(chunks.len() >= 3);
        for c in &chunks {
            assert!(c.chars().count() <= SYNTH_CHUNK_LEN + 1, "oversize: {c}");
        }
        // Nothing lost: every word survives across the chunk boundary.
        let joined = chunks.join(" ");
        assert!(joined.contains("Corta."));
        assert!(joined.contains("Fin."));
    }

    #[test]
    fn speaking_series_is_actives_then_one_inactive() {
        assert_eq!(
            speaking_sequence(3),
            vec![
                SpeakingPoint { active: true, chunk: 1, chunks: 3 },
                SpeakingPoint { active: true, chunk: 2, chunks: 3 },
                SpeakingPoint { active: true, chunk: 3, chunks: 3 },
                SpeakingPoint { active: false, chunk: 3, chunks: 3 },
            ]
        );
        // Single-chunk utterances still open AND close the series.
        assert_eq!(
            speaking_sequence(1),
            vec![
                SpeakingPoint { active: true, chunk: 1, chunks: 1 },
                SpeakingPoint { active: false, chunk: 1, chunks: 1 },
            ]
        );
    }

    #[test]
    fn wav_decode_roundtrips_daemon_files() {
        let dir = std::env::temp_dir().join("smartpc-playback-wav");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("roundtrip.wav");
        let samples: Vec<i16> = vec![0, 16384, -16384, 32767, -32768];
        write_test_wav(&path, &samples, 22050);
        let decoded = decode_wav_16(&path).unwrap();
        assert_eq!(decoded.rate, 22050);
        assert_eq!(decoded.samples, samples);
        // Garbage fails closed (never reaches the speaker).
        std::fs::write(dir.join("bad.wav"), b"not audio at all......................").unwrap();
        assert!(decode_wav_16(&dir.join("bad.wav")).is_err());
        std::fs::write(dir.join("empty.wav"), b"RIFF\x00\x00\x00\x00WAVE").unwrap();
        assert!(decode_wav_16(&dir.join("empty.wav")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resample_is_identity_at_device_rate_and_dupes_channels() {
        let stereo = resample_to_device(&[16384, -16384], 22050, 22050, 2);
        assert_eq!(stereo.len(), 4);
        assert!((stereo[0] - 0.5).abs() < 1e-6);
        assert!((stereo[1] - 0.5).abs() < 1e-6); // mono duplicated
        assert!((stereo[2] + 0.5).abs() < 1e-6);
        // 22050 → 44100 doubles the frame count, endpoints pinned.
        let up = resample_to_device(&[0, 32767], 22050, 44100, 1);
        assert_eq!(up.len(), 4);
        assert!(up[0].abs() < 1e-6);
        assert!((up[3] - 1.0).abs() < 0.01);
        assert!(resample_to_device(&[], 22050, 44100, 2).is_empty());
        assert!(resample_to_device(&[1], 22050, 22050, 0).is_empty());
    }

    #[test]
    fn live_sink_stop_is_idempotent() {
        let live = LiveSink::new();
        assert!(!live.playing());
        live.stop();
        live.stop();
        assert!(live.get().is_none());
        let handle = Arc::new(PlayHandle::new(Arc::new(AtomicBool::new(false))));
        live.set(handle.clone());
        assert!(live.playing());
        live.clear_if_same(&Arc::new(PlayHandle::new(Arc::new(AtomicBool::new(false)))));
        assert!(live.playing()); // another turn's handle is never dropped
        live.clear_if_same(&handle);
        assert!(!live.playing());
        live.stop(); // idempotent on an empty slot
    }

    /// Live-device test only: needs a real output (speakers or a virtual
    /// sink). Default CI asserts queue math + the event series above and
    /// never touches a device. Run live with SMARTPC_AUDIO_LIVE=1.
    #[test]
    fn live_output_plays_silence_to_completion() {
        if std::env::var("SMARTPC_AUDIO_LIVE").unwrap_or_default() != "1" {
            return;
        }
        let dir = std::env::temp_dir().join("smartpc-playback-live");
        let _ = std::fs::create_dir_all(&dir);
        let wav = dir.join("silence.wav");
        write_test_wav(&wav, &vec![0i16; 2205], 22050); // 0.1 s silence
        let stop = Arc::new(AtomicBool::new(false));
        let opened = open_output(&[wav], &stop).expect("live audio device");
        assert!(opened.total_frames > 0);
        assert_eq!(opened.boundaries.len(), 1);
        opened.stream.play().unwrap();
        let t0 = Instant::now();
        while opened.consumed.load(Ordering::Relaxed) < opened.total_frames {
            assert!(t0.elapsed() < Duration::from_secs(10), "live playback stalled");
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
