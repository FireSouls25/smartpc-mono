# 09 — Local voice input (mic → VAD → whisper → agent)

Implemented 2026-09-18. Press (or say "hey"), talk, pause — the silence
finalizes the utterance, whisper transcribes on-device, the text is sent to
the agent as if typed. No finish button, no finish keyword, no network.

## Pipeline

```text
cpal mic ──► 16 kHz mono f32 ──► energy VAD ──► utterance (+300 ms pre-roll)
                                                        │
                         wake mode: transcribe onset ──► wake word in it?
                                                        │ yes → capture NEXT
                                                        │       utterance
                                                        ▼
manual mode: first utterance ──► transcribe ──► event + auto-stop
wake mode:   command utterance ──► transcribe ──► event, re-arm
```

`src/native/src/stt/`: `audio` (capture, mixdown, linear resample),
`vad` (threshold + min-speech gate + silence hangover), `wake` (text match),
`model` (naming, path, download), `engine` (whisper-rs wrapper),
`session` (one global session, listener thread, event queue), `routes`.

## Modes

- **manual** (default): mic button starts, first utterance transcribes,
  session ends by itself. Starting is a press; finishing never is.
- **wake**: armed until the wake word ("hey" default, editable in
  Settings → Voice, 1–32 chars) appears in a transcribed onset utterance;
  the next utterance is the command; re-arms until stopped.

Both modes are also driven by the push-to-talk shortcut (default
`Ctrl+M`, changeable in Settings → Voice): it toggles the mic from
anywhere, and the mic button tooltip always names the current key.

## Conversation mode (continuous talk)

A third mode, `conversation`, loops single utterances without further
presses: listen → agent → TTS reply → listen again (half-duplex —
there is no echo cancellation in this stack, so turns stay strictly
sequential: talk, hear the reply, talk). Details:

- TTS is first-class here: replies are always spoken, independent of the
  read-aloud toggle (normal manual/wake mode stays mute unless opted in).
- Echo guard: a transcript matching the just-spoken reply (speaker bleed
  into the mic) is dropped and the loop resumes instead of answering
  itself. Short overlaps (< 12 chars) never count.
- Barge-in is the toggle/hotkey: it cuts speech AND ends the loop.
- Safety rails: 30 turns max, agent-busy parks words in the composer and
  ends the loop, TTS failure stops honestly instead of looping mute.
- Backend `conversation` behaves like `manual` per turn (one utterance →
  transcript → end); the renderer owns the chaining.

Wake detection is textual, not acoustic: VAD-gated onsets are transcribed
with tiny and split into match + remainder (`split_wake_command`).
Whole-word for single words ("they" ≠ "hey"; "¡Hey!" = "hey"), substring
for phrases. Cost: one tiny transcription per speech onset while armed —
nothing runs continuously. Limitation, stated: any mention wakes, background
chatter costs CPU, accents are significant.

## Silence finalize

VAD frames are 30 ms RMS at 16 kHz. Defaults: threshold `0.02`,
`VOICE_SILENCE_MS=1200` hangover, `VOICE_MIN_SPEECH_MS=400` gate (clicks
and coughs never start an utterance), `VOICE_MAX_UTTERANCE_S=30` cap.
All env-overridable (`VOICE_THRESHOLD` is raw RMS). Pre-roll keeps the
~300 ms before onset so the first syllable survives the gate. The start
gate is M-of-N with a `VOICE_GAP_MS=120` tolerance: speech hovering around
the threshold (vowels over, consonants under) accumulates hits across
brief dips — only a sustained gap abandons the run. The session summary
reports `best run H/N` so triage tells "too quiet" (0–2) from "hovering
at the gate" (near N).

## Model (low-hardware first)

Default `tiny` (~75 MB): 11 s of speech transcribes in ~1.5 s on a laptop
CPU (measured 2026-09-18, realtime factor ~0.14×). `WHISPER_MODEL` picks
`tiny|tiny.en|base|base.en|small`; `WHISPER_THREADS` caps CPU (default
min(4, cpus)); language per session (`es` default, from the UI lang).
Settings → Voice offers the same model list per session (first use
downloads, later listens are instant) plus a sensitivity control
(Baja/Media/Alta → VAD threshold 0.035/0.02/0.01; Media omits the override
so `VOICE_THRESHOLD` keeps working). If speech peaks below the threshold,
raise sensitivity before touching OS gain — the session summary line
(`max rms …`) tells you exactly where you landed.
First use downloads from HuggingFace into `<db-dir>/models/`
(`WHISPER_MODEL_DIR` overrides); progress goes to diagnostics. Greedy,
single-segment, blank-suppressed: commands want latency, not poetry.

## HTTP contract (sidecar-gated, like providers)

- `GET /v1/voice/status` → `{listening, capturing, mode, model, wake_word,
mic, model_ready}`. Never blocks on audio.
- `POST /v1/voice/listen {mode, wake_word?, lang?, model?}` → `{ok}`.
  Blocks on first-use download (client budget 3 min). Errors:
  `already_listening` 409, `no_microphone` 503, `invalid_mode|
invalid_wake_word|invalid_lang` 400, `model_failed` 502.
- `POST /v1/voice/stop` → `{ok}` always (idempotent).
- `GET /v1/voice/events?cursor=N` → `{events, next}`, holds ~25 s.
  Events: `started{}`, `capturing{active}`, `wake{word}`, `transcript{text}`,
  `error{code,message}`, `end{}` — each with `seq` + session `epoch`.

## Frontend (`domains/voice/`)

`voice.api.ts` (typed client) + `voice.store.svelte.ts` (prefs in
localStorage, poll loop with cursor + generation counter so stale async work
stands down, transcript routing). Final transcripts call `chat.send()` —
they render as user bubbles and run the agent identically to typed text; if
the agent is busy the words land in the composer instead of being lost.
Orb: capturing → `listening`; handoff leaves the agent's `thinking` alone.
Mic button toggles the preferred mode; Settings → Voice holds mode, wake
word, input picker, and the read-aloud toggle. Prefs: `smartpc.voice.mode`,
`smartpc.voice.wake`, `smartpc.voice.device`, `smartpc.voice.speak`.

## Session integrity (learned from a real stuck-session bug)

The event queue is global across sessions, so every event carries its
session `epoch` (`listen` returns it): clients drop foreign-epoch events
instead of acting on them, and `start()` clears the queue. Without this, a
new poller starting at cursor 0 replayed the previous session's `End`,
killed its own poll loop instantly, and orphaned a live session that then
409'd every later listen — while never delivering transcripts.

Every error is terminal and every exit clears exactly its own record:

- `stop()` takes the record immediately (recovery works even if the thread
  already died) and flags the thread; the thread always emits the single
  `End` on exit. No double-Ends, no orphans.
- Listener exits are epoch-guarded: a slow death can never wipe a newer
  session's record, so stop→listen in quick succession is safe.
- A panicking listener is caught: `Error{voice_crash}` + `End` + release.
- The frontend treats errors as terminal too: it forces `stop()` cleanup
  before showing the message, so the next press always starts fresh
  (previously: permanent `already_listening` 409s).
- The mic button is disabled while `starting` (a press mid-download used to
  cancel the very listen it was waiting for).

## Platforms & build

- Capture backends: ALSA (Linux), CoreAudio (macOS), WASAPI (Windows) via
  cpal — no per-OS code in the app. Mic permission is the OS dialog (macOS
  consent, PipeWire portal); browsers are out of the loop (the sidecar
  captures, not the page).
- Build needs: cmake + C++ compiler (whisper.cpp compiles from bundled
  sources) + `libasound2-dev` on Linux. CI installs both (see `ci.yml`).
- Verified live 2026-09-18: real DMIC present, tiny downloaded, session
  start/status/poll/stop all green against hardware. Acoustic loopback was
  impossible here (HDMI/speakers silent, mic at noise floor), so
  transcription was proven on the JFK sample through the same engine path.

## Devices: picker, not prayers

`GET /v1/voice/status` reports `device` (current), `mic`, and `inputs[]`
(every _capturable_ endpoint — playback outputs are filtered by probing for
input configs). Settings → Voice has an input dropdown ("System default" +
list); the choice persists (`smartpc.voice.device`) and rides in
`POST /v1/voice/listen {device?}`. Unknown names 400 (`invalid_device`).

Two hard-won details (plus two from 2026-09-22):

- Same-name hardware appears once per subdevice: candidates are tried in
  order with **mic-hint ranking** (DMIC/microphone/headset first, HDMI/
  output/monitor last), because a silent line-in that opens fine is worse
  than an honest error. Names compare trimmed (ALSA pads whitespace).
- "System default" no longer means cpal's default PCM (on PipeWire-via-ALSA
  it opens fine and then delivers zero frames — a silent stall). With no
  device picked the sidecar opens the best-ranked live input; the OS mixer
  (PipeWire/Pulse, which follows the default source) outranks raw nodes.
  The session-start diagnostics line names the device that **actually
  opened** (`dev=…`), with its real format (`2ch@48000Hz "F32"`).
- Virtual sinks ("Discard all samples…", `null`) are filtered out of the
  picker entirely: the null sink floods zero frames at full speed, so the
  VAD never fires and manual sessions never finalize.
- The picker dedupes by value: raw ALSA names repeat per subdevice and
  duplicate keys crash Svelte's keyed each block (empty menu, `each_key`
  error — covered by an E2E regression test asserting options render,
  selection persists, and no key errors). Server-side matching is unaffected
  (every same-name candidate is still tried).

## Debugging "nothing arrives" (read diagnostics top-down)

Settings → AI → diagnostics mirrors the pipeline stages; find the last line
present and the break is the next stage:

1. `voice: listening (…, dev=<name>, vad thr=…)` — session alive, mic open.
   If `dev=` is a placeholder ("Default Audio Device") or the wrong hardware,
   pick the real input in Settings → Voice → Entrada.
2. `voice: audio flowing, first frame rms=…` — frames arrive at all. Absent
   means the stream stalled (report it). `rms=0.0000` forever means digital
   silence: muted at OS level or a dead subdevice — try another Entrada.
   A quiet room reads ~0.001–0.005; that is normal, not broken.
3. `voice: idle level rms=… max=…` (every ~15 s while waiting) — speak and
   watch: if the max never approaches the threshold while you talk loudly,
   the mic gain is too low (OS volume) or `VOICE_THRESHOLD` too high
   (try `0.01`). If it fires constantly with no speech, threshold too low.
   Always check the OS gain first: `wpctl get-volume @DEFAULT_AUDIO_SOURCE@`
   (Linux) — 33 % (≈ −29 dB) buries speech under any threshold; 100 % is
   the sane baseline before touching sensitivity.
4. `voice: speech detected, capturing…` — VAD hears you.
5. `voice: transcribing N samples…` + `voice: heard '…'` — whisper ran.
   `(empty)` means the VAD fired on noise; check stage 3 tuning.
6. Wake mode: `voice: no wake word in onset, re-arming ('…')` — the quoted
   text is what whisper heard; if it never contains your wake word, say it
   first and alone, or check the language (`lang=` in line 1).
7. Otherwise the transcript event fired — the break is renderer-side
   (poll loop); that path is epoch-guarded and contract-tested.

`t()` supports `{var}` interpolation for strings like the armed hint
(`Di «{word}» para empezar`), added for the wake-mode "say hey" cue.

## Limits & next

- No partial transcripts (whisper is batch; VAD level could feed a meter).
- One global mic session; TTS speaks per-utterance disposable children.
- Wake word is single-phrase text match — a proper acoustic spotter
  (openWakeWord/Porcupine) is the upgrade if false-wake cost ever matters.
- 26 Rust unit tests (VAD machine incl. introspection, wake match + split,
  resample/mixdown, model map, `parse_opts`, session stop idempotency, TTS
  estimate/voices);
  contract pins status shape + listen/speak validation + epoch wire; E2E covers the
  Voice settings section (headless has no mic — asserts graceful `notReady`).

## TTS output (pi-listen engine, disposable children)

Spikes proved two things that shape the design: extension commands never
emit turn lifecycle events in RPC mode (no `agent_start`/`agent_settled` —
verified with a trivial command too), and a finished engine sometimes keeps
its child alive on leaked handles. So each utterance gets a FRESH pi child:
prompt `/voice-speak`, drain stdout (never block the pipe), watchdog-SIGKILL
after the duration estimate (~14 chars/sec + 20 s, 25–240 s). Leak-proof by
construction; `/voice-speak-stop` (or any new speak) barges in by killing.

The child runs with an isolated `HOME` (`<data>/pi/tts-home`, our own
`settings.json`: TTS enabled, local backend, per-language voice) — never
the user's real pi setup. Voices: Piper MIT per language (`piper-es_ES-davefx`,
`piper-fr_FR-siwis`, … — the full `piper-` ids pi-listen requires; bare
Piper names are rejected as unknown) defaulting to Kitten Nano EN. First use
downloads ~21 MB.
History stays clean (verified: 0 messages before/after a speak).
`POST /v1/voice/speak {text, lang?}` → `{ok, estimated_ms}` (400 on empty /

> 2000 chars); `POST /v1/voice/speak-stop` always ok. UI: Settings toggle
> (`smartpc.voice.speak`), auto-speak on fresh replies only (pub/sub from the
> send path — history loads never fire), speaking indicator with click-stop,
> submit barges in.
