//! Mic capture (cpal) + PCM plumbing to 16 kHz mono f32, whisper's diet.
//!
//! One code path everywhere: default input → whatever rate/channels the OS
//! gives → mix down + linear-resample in the callback. Callbacks must never
//! block, so full 30 ms frames go through a bounded channel (overflow drops,
//! underruns can't happen — the VAD just sees a gap).
use std::sync::mpsc::SyncSender;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

use super::vad::FRAME_SAMPLES;

/// Whisper's native rate. Everything converges here.
pub const TARGET_RATE: u32 = 16000;

#[derive(Debug)]
pub enum CaptureError {
    NoMicrophone,
    Unsupported(String),
    Stream(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoMicrophone => write!(f, "no microphone found"),
            Self::Unsupported(e) => write!(f, "unsupported audio config: {e}"),
            Self::Stream(e) => write!(f, "audio stream failed: {e}"),
        }
    }
}

pub fn microphone_present() -> bool {
    cpal::default_host().default_input_device().is_some()
}

/// Human-readable input device name for diagnostics ("are we listening to
/// the right mic?"). None when there is no input device at all.
pub fn input_device_name() -> Option<String> {
    cpal::default_host()
        .default_input_device()
        .and_then(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
        .filter(|n| !n.trim().is_empty())
}

/// All *capturable* input device names (for the settings picker). Playback
/// endpoints (HDMI outputs, upmix plugins…) are filtered by probing for at
/// least one supported input config — a name you can pick but never open is
/// worse than no name. Empty when the host reports none — never an error:
/// absence is data, not failure.
pub fn list_input_devices() -> Vec<String> {
    ranked_input_devices()
        .into_iter()
        .filter_map(|d| d.description().ok())
        .map(|desc| desc.name().to_string())
        .filter(|n| !n.trim().is_empty())
        .collect()
}

/// OS mixer level for the capture path, when the platform exposes one.
///
/// Read-only by design: the app never changes OS gain itself (that would
/// surprise), it only reports it — a mic at 33 % buries speech under any
/// VAD threshold, and without this the diagnostics just say "too quiet".
/// Linux reads PipeWire (`wpctl`); other platforms return `None`
/// (macOS/Windows give no stable CLI for input gain).
#[derive(Debug, Clone, serde::Serialize)]
pub struct MixerState {
    /// 0.0–1.0+ (PipeWire allows >100 %).
    pub volume: f32,
    pub muted: bool,
}

pub fn mixer_state() -> Option<MixerState> {
    #[cfg(target_os = "linux")]
    {
        linux_mixer_state()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

#[cfg(target_os = "linux")]
fn linux_mixer_state() -> Option<MixerState> {
    // `timeout` guards a hung sound server; failure (no wpctl, no PipeWire)
    // is absence, not error — the picker/diagnostics carry on regardless.
    let out = std::process::Command::new("timeout")
        .args(["5", "wpctl", "get-volume", "@DEFAULT_AUDIO_SOURCE@"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // "Volume: 0.33" or "Volume: 0.33 [MUTED]".
    let (volume, muted) = parse_wpctl_volume(&String::from_utf8_lossy(&out.stdout))?;
    Some(MixerState { volume, muted })
}

#[cfg(target_os = "linux")]
fn parse_wpctl_volume(text: &str) -> Option<(f32, bool)> {
    let rest = text.trim().strip_prefix("Volume:")?.trim();
    let (level, muted) = match rest.split_once(' ') {
        Some((v, flag)) => (v, flag.contains("MUTED")),
        None => (rest, false),
    };
    Some((level.parse().ok()?, muted))
}
/// What the opened stream actually runs at (logged: sample-rate surprises
/// are a classic "VAD hears nothing useful" cause).
#[derive(Debug, Clone)]
pub struct CaptureDesc {
    pub device: String,
    pub rate: u32,
    pub channels: usize,
    pub format: String,
}

pub fn rms(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return 0.0;
    }
    let sum: f32 = frame.iter().map(|s| s * s).sum();
    (sum / frame.len() as f32).sqrt()
}

/// Linear resample mono f32. Good enough for voice (whisper is robust to
/// resampling artifacts); avoids pulling a DSP crate for one call site.
pub fn resample_linear(input: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate || input.is_empty() {
        return input.to_vec();
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((input.len() as f64) / ratio).ceil() as usize;
    (0..out_len)
        .map(|i| {
            let pos = i as f64 * ratio;
            let i0 = pos.floor() as usize;
            let frac = (pos - i0 as f64) as f32;
            let s0 = input.get(i0).copied().unwrap_or(0.0);
            let s1 = input.get(i0 + 1).copied().unwrap_or(s0);
            s0 + (s1 - s0) * frac
        })
        .collect()
}

fn i16_to_f32(v: i16) -> f32 {
    v as f32 / 32768.0
}

/// Rank a device description for capture likelihood: mic-looking hardware
/// first, anything output-smelling last. Lower is better. The OS mixer
/// (PipeWire/Pulse) leads: it follows the user's default source, which is
/// what "System default" means — while ALSA's `default` PCM sometimes opens
/// fine and then never delivers a frame.
fn mic_hint_score(haystack_lower: &str) -> usize {
    const HINTS: &[&str] = &[
        "pipewire",
        "pulse",
        "dmic",
        "microphone",
        "headset",
        "mic",
        "input",
        "capture",
        "array",
    ];
    const ANTI_HINTS: &[&str] = &["hdmi", "output", "speaker", "playback", "monitor"];
    if ANTI_HINTS.iter().any(|h| haystack_lower.contains(h)) {
        return usize::MAX - 1;
    }
    HINTS
        .iter()
        .position(|h| haystack_lower.contains(h))
        .unwrap_or(HINTS.len())
}

/// Virtual sinks that open fine but can never hear a room: the null sink
/// floods zero frames at full speed (the VAD never fires, sessions never
/// finalize), and monitor sources only mirror an output. Neither is listed
/// nor auto-picked.
fn is_sink_name(name: &str) -> bool {
    let n = name.trim().to_lowercase();
    n == "null"
        || n.contains("discard")
        || n.contains("zero samples")
        || n.contains("monitor")
}

fn device_score(d: &cpal::Device) -> usize {
    d.description()
        .ok()
        .map(|desc| {
            let hay = format!(
                "{} {}",
                desc.name(),
                desc.extended().collect::<Vec<_>>().join(" ")
            )
            .to_lowercase();
            mic_hint_score(&hay)
        })
        .unwrap_or(usize::MAX)
}

/// Every capturable, non-sink input, mic-likely first. Empty when the host
/// reports none — never an error: absence is data, not failure.
fn ranked_input_devices() -> Vec<cpal::Device> {
    let mut all: Vec<(usize, cpal::Device)> = cpal::default_host()
        .input_devices()
        .map(|devices| {
            devices
                .filter(|d| {
                    d.description().is_ok_and(|desc| {
                        let n = desc.name();
                        !n.trim().is_empty() && !is_sink_name(n)
                    })
                })
                .filter(|d| {
                    d.supported_input_configs()
                        .map(|mut it| it.next().is_some())
                        .unwrap_or(false)
                })
                .map(|d| (device_score(&d), d))
                .collect()
        })
        .unwrap_or_default();
    all.sort_by_key(|(score, _)| *score);
    all.into_iter().map(|(_, d)| d).collect()
}

fn u16_to_f32(v: u16) -> f32 {
    v as f32 / 32768.0 - 1.0
}

/// Batches device-rate mono into 16 kHz 480-sample frames. One per stream
/// callback closure (owned, `'static`); overflow drops, never blocks audio.
struct Framer {
    tx: SyncSender<Result<Vec<f32>, String>>,
    leftover: Vec<f32>,
    from_rate: u32,
}

impl Framer {
    fn push(&mut self, mono_at_device_rate: &[f32]) {
        self.leftover.extend(resample_linear(
            mono_at_device_rate,
            self.from_rate,
            TARGET_RATE,
        ));
        while self.leftover.len() >= FRAME_SAMPLES {
            let frame: Vec<f32> = self.leftover.drain(..FRAME_SAMPLES).collect();
            let _ = self.tx.try_send(Ok(frame));
        }
    }
}

fn framer(tx: &SyncSender<Result<Vec<f32>, String>>, from_rate: u32) -> Framer {
    Framer {
        tx: tx.clone(),
        leftover: Vec::with_capacity(FRAME_SAMPLES * 4),
        from_rate,
    }
}

/// Open a mic for capture. `wanted`: `None`/empty = system default,
/// otherwise an exact device name from [`list_input_devices`].
/// Frames arrive as `Ok([f32; 480])`; stream failures arrive as
/// `Err(message)` and the caller must shut down.
pub fn open_capture(
    tx: SyncSender<Result<Vec<f32>, String>>,
    wanted: Option<&str>,
) -> Result<(cpal::Stream, CaptureDesc), CaptureError> {
    let host = cpal::default_host();
    // Same-name hardware appears once per subdevice/direction: try every
    // match in order (the first "sof-hda-dsp" may be output-only or busy)
    // and report the last error only if none opens. Names compare trimmed:
    // ALSA pads some with trailing whitespace.
    let candidates: Vec<cpal::Device> = match wanted
        .filter(|w| !w.trim().is_empty())
    {
        Some(name) => {
            let mut all: Vec<(usize, cpal::Device)> = host
                .input_devices()
                .map_err(|e| CaptureError::Unsupported(e.to_string()))?
                .filter(|d| {
                    d.description()
                        .is_ok_and(|desc| desc.name().trim() == name.trim())
                })
                .map(|d| (device_score(&d), d))
                .collect();
            if all.is_empty() {
                return Err(CaptureError::Unsupported(format!(
                    "input device not found: {name}"
                )));
            }
            // Mic-looking subdevices (DMIC, headset, "mic", …) first: a
            // silent line-in that opens fine is worse than useless.
            all.sort_by_key(|(score, _)| *score);
            all.into_iter().map(|(_, d)| d).collect()
        }
        None => {
            // No pick: the best-ranked live input, not cpal's default —
            // on PipeWire-via-ALSA systems the default PCM opens fine and
            // then never delivers a frame (silent stall). Empty only when
            // the host reports no inputs at all (headless/CI): keep the
            // honest NoMicrophone path via the cpal default.
            let ranked = ranked_input_devices();
            if ranked.is_empty() {
                vec![host.default_input_device().ok_or(CaptureError::NoMicrophone)?]
            } else {
                ranked
            }
        }
    };
    let mut last_err: Option<CaptureError> = None;
    for device in candidates {
        match open_device(device, &tx) {
            Ok(ok) => return Ok(ok),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or(CaptureError::NoMicrophone))
}

/// Open one concrete device (factored so same-name candidates each get tried).
fn open_device(
    device: cpal::Device,
    tx: &SyncSender<Result<Vec<f32>, String>>,
) -> Result<(cpal::Stream, CaptureDesc), CaptureError> {
    let desc = device
        .description()
        .ok()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| "?".to_string());
    let config = device
        .default_input_config()
        .map_err(|e| CaptureError::Unsupported(e.to_string()))?;
    let from_rate = config.sample_rate();
    let channels = config.channels() as usize;
    if channels == 0 {
        return Err(CaptureError::Unsupported("0 channels".to_string()));
    }
    let stream_config: cpal::StreamConfig = config.config();
    let format = format!("{:?}", config.sample_format());
    // (*tx).clone(): clone the sender itself, not the reference.
    let err_tx = (*tx).clone();
    let err_fn = move |err| {
        let _ = err_tx.send(Err(format!("{err}")));
    };
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => {
            let mut framer = framer(tx, from_rate);
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[f32], _| {
                    framer.push(&mix_down(data, channels));
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I16 => {
            let mut framer = framer(tx, from_rate);
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[i16], _| {
                    let conv: Vec<f32> =
                        data.iter().map(|v| i16_to_f32(*v)).collect();
                    framer.push(&mix_down(&conv, channels));
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::U16 => {
            let mut framer = framer(tx, from_rate);
            device.build_input_stream(
                stream_config.clone(),
                move |data: &[u16], _| {
                    let conv: Vec<f32> =
                        data.iter().map(|v| u16_to_f32(*v)).collect();
                    framer.push(&mix_down(&conv, channels));
                },
                err_fn,
                None,
            )
        }
        f => {
            return Err(CaptureError::Unsupported(format!(
                "sample format {f:?}"
            )));
        }
    }
    .map_err(|e| CaptureError::Stream(e.to_string()))?;
    stream
        .play()
        .map_err(|e| CaptureError::Stream(e.to_string()))?;
    Ok((
        stream,
        CaptureDesc {
            device: desc,
            rate: from_rate,
            channels,
            format,
        },
    ))
}

/// Mix N channels to mono, then resample to 16 kHz. `data` is interleaved.
fn mix_down(data: &[f32], channels: usize) -> Vec<f32> {
    if data.is_empty() {
        return Vec::new();
    }
    let frames = data.len() / channels;
    let mut mono = Vec::with_capacity(frames);
    for i in 0..frames {
        let mut sum = 0.0;
        for c in 0..channels {
            sum += data[i * channels + c];
        }
        mono.push(sum / channels as f32);
    }
    mono
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc::sync_channel;

    #[test]
    fn rms_measures_energy() {
        assert_eq!(rms(&[]), 0.0);
        assert!((rms(&[0.5, -0.5, 0.5, -0.5]) - 0.5).abs() < 1e-6);
        assert!(rms(&[0.0; 480]) < 1e-9);
    }

    #[test]
    fn resample_is_identity_at_target_rate() {
        let v = vec![0.1, 0.2, 0.3];
        assert_eq!(resample_linear(&v, 16000, 16000), v);
        assert!(resample_linear(&[], 48000, 16000).is_empty());
    }

    #[test]
    fn resample_halves_length_and_keeps_endpoints() {
        let v: Vec<f32> = (0..480).map(|i| i as f32 / 480.0).collect();
        let out = resample_linear(&v, 48000, 16000);
        assert_eq!(out.len(), 160);
        assert!((out[0] - 0.0).abs() < 1e-6);
        // Linear interp lands within one input step of the true ramp.
        for (i, s) in out.iter().enumerate() {
            let want = (i * 3) as f32 / 480.0;
            assert!((s - want).abs() < 1.0 / 480.0 + 1e-6, "{i}: {s} vs {want}");
        }
    }

    #[test]
    fn mix_down_averages_channels() {
        assert_eq!(mix_down(&[1.0, 3.0, 2.0, 4.0], 2), vec![2.0, 3.0]);
        assert_eq!(mix_down(&[0.5, 0.5], 1), vec![0.5, 0.5]);
        assert!(mix_down(&[], 2).is_empty());
    }

    #[test]
    fn sample_conversions_span_minus_one_to_one() {
        assert!((i16_to_f32(32767) - 1.0).abs() < 1e-4);
        assert!((i16_to_f32(-32768) + 1.0).abs() < 1e-4);
        assert!((u16_to_f32(65535) - 1.0).abs() < 1e-4);
        assert!((u16_to_f32(0) + 1.0).abs() < 1e-4);
    }

    #[test]
    fn channel_capacity_constant_is_sane() {
        let (tx, rx) = sync_channel::<Result<Vec<f32>, String>>(128);
        tx.try_send(Ok(vec![0.0; FRAME_SAMPLES])).unwrap();
        assert_eq!(rx.recv().unwrap().unwrap().len(), FRAME_SAMPLES);
    }

    #[test]
    fn mic_hints_rank_dmic_first_and_hdmi_last() {
        assert!(mic_hint_score("sof-hda-dsp dmic16khz digital mic") < mic_hint_score("line in"));
        assert!(mic_hint_score("usb microphone headset") < mic_hint_score("plain thing"));
        assert_eq!(
            mic_hint_score("hdmi output monitor"),
            usize::MAX - 1
        );
        assert!(mic_hint_score("default audio device") < usize::MAX - 1);
    }

    #[test]
    fn mixer_leads_generic_and_sinks_are_spotted() {
        // "System default" should follow the OS mixer, not a random node.
        assert!(mic_hint_score("pipewire sound server") < mic_hint_score("jack audio connection kit"));
        assert!(mic_hint_score("pulseaudio sound server") < mic_hint_score("usb headset"));
        assert!(is_sink_name(
            "Discard all samples (playback) or generate zero samples (capture)"
        ));
        assert!(is_sink_name("null"));
        assert!(is_sink_name("HDMI monitor"));
        assert!(!is_sink_name("PipeWire Sound Server"));
        assert!(!is_sink_name("sof-hda-dsp, "));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn wpctl_volume_parses_levels_and_mute() {
        assert_eq!(parse_wpctl_volume("Volume: 0.33\n"), Some((0.33, false)));
        assert_eq!(
            parse_wpctl_volume("Volume: 1.00 [MUTED]\n"),
            Some((1.0, true))
        );
        assert_eq!(parse_wpctl_volume("Volume: 1.50\n"), Some((1.5, false)));
        assert_eq!(parse_wpctl_volume("nonsense"), None);
        assert_eq!(parse_wpctl_volume(""), None);
    }
}
