# 13 — A2′ TTS engine: owned synth (sherpa) + owned playback (Rodio)

Status: SPEC (spike-proven 2026-10-08, build on approval — below is the wave
brief). Replaces the disposable-pi-child `/voice-speak` path; pi-listen
stays an npm library (its engine), never a runtime child for TTS.

## Proven recipe (do not redesign — implement this)

Upstream Piper voices (rhasspy/piper-voices) load in sherpa vits iff:
1. `.onnx` + `.onnx.json` downloaded (json = value source only).
2. ONNX metadata appended (protobuf field 14 entries): `sample_rate`
   (json `audio.sample_rate`), `n_speakers` (json `num_speakers`),
   `language` + `voice` (json `espeak.voice` base, e.g. `es`),
   `comment` containing the substring `piper` (sherpa sets `is_piper`
   by substring match — verified in sherpa source).
3. Shared `tokens.txt` (espeak phone table — copy once) + shared
   `espeak-ng-data/` (copy once) alongside every voice.
4. `sid` per `speaker_id_map` (sharvard M:0/F:1; single-speaker: 0).
Proven live: Davefx (native metadata), Sharvard sid 0+1 (patched),
Daniela sid 0 (patched) — all healthy RMS, ~200–1200 ms synth.

## Voice table (final)

- es: Davefx M (default, existing cache untouched), Sharvard M (sid 0),
  Sharvard F (sid 1) — one 76 MB model, `kokoro` gone for good;
  Daniela F (es-AR accent, 114 MB, sid 0). = 2M + 2F. ✅
- en: Kitten M1/M2/F1/F2 (sids 0/2/1/3, existing cache). Unchanged.
- No other languages (operator decision stands).
- Old stored prefs (`kokoro…`, plain kitten id) already fall back via the
  invalid_voice path — no migration needed.

## Architecture

```
renderer ──HTTP──► sidecar ──stdio JSONL──► node tts-synth.js (daemon)
  speak/                           │  {id,cmd:ensure|synth|stop}       │ sherpa-onnx-node
  catalog/download/delete/events   │  {id,ok,…} / wav files           │ (vendored prebuilt)
  speaking{active} events ◄─────────┘
  Rodio Sink playback (sidecar) — completion is REAL (no watchdog guess)
```

- **Daemon** (`pi-bridge/tts-synth.js`, zero new npm deps): line-protocol
  JSON on stdio; `ensure` downloads (+patches+tokens+data, idempotent,
  per-model lock); `synth` writes `<model>/<sid>-<hash>.wav`, returns path;
  `stop`/`quit`. Resolves its own sherpa lib dir at runtime
  (`sherpa-onnx-<platform>-<arch>`; Linux LD_LIBRARY_PATH / macOS DYLD set
  by the spawner). Spawned lazily on first speak/ensure, reaped on
  sidecar drop + `speak-stop` kills playback only (daemon persists).
- **Rust** (`tts.rs` rewrite + new `playback.rs`): `speak()` =
  resolve → daemon ensure (first-use download, client 180 s budget) →
  daemon synth per chunk → Rodio `Sink` queue → real end events.
  `TtsManager` keeps its method shapes; internals swap child-spawn for
  daemon+Sink. Watchdog stays as backup only (log, don't reap live audio).
  `write_tts_config` + isolated pi home: DELETED (nothing configures pi).
- **Events**: `speaking{active,chunk,chunks,model}` on the existing
  `/v1/voice/events` poll (new variant, additive). Frontend replaces the
  optimistic timer with it; highlight advance becomes exact.
- **Cache**: `<tts-home>/.pi/models/tts/<our-id>/`
  (`model.onnx`, `model.onnx.json`, `tokens.txt`, `espeak-ng-data/`);
  `_shared/` holds one canonical tokens.txt + espeak-ng-data, copied per
  voice at ensure time (copies, not symlinks — Windows-safe). Existing
  davefx/kitten caches are reused as-is (davefx seeds `_shared`).
  DELETE removes the voice dir (shared stays). `ready` = model.onnx present.
- **Catalog** (`TTS_CATALOG`): sharvard×2 (`upstream-piper-es-sharvard#0/1`,
  76 MB shared note), daniela×1 (`upstream-piper-es-daniela`, 114 MB),
  kitten×4, davefx — each with gender + per-voice `url` + `size_mb`.
  Download URLs are rhasspy/piper-voices `resolve/v1.0.0/…` (pinned tag).
- **No PROTOCOL bump**: no new routes (download rides test-play/speak;
  explicit Download button calls speak-canned-phrase as today —
  wait, explicit Download per row exists (downloadTtsVoice). It calls
  speak(test phrase) → ensure downloads → synth proves. Unchanged flow,
  new engine underneath. DELETE + catalog shapes unchanged (ids change
  value, not shape). If a `download` endpoint gets added, bump then.

## What is deleted

- Disposable pi-child TTS path (`speak()` spawn, drain task, watchdog
  reap, `write_tts_config`, isolated tts-home config, `tts_model_for_lang`
  piper-id mapping stays as DEFAULTS only).
- The `pi-bridge` TTS dependency at runtime (sherpa lib remains).
- NOT deleted: whisper/STT, voice store queue/pump, Settings panel
  structure, catalog endpoint shape, `invalid_voice` fail-loud contract.

## New deps (final)

- Rust: `rodio 0.20` ONLY (cpal backend; ALSA already required — zero new
  system libs on any OS). No onnxruntime/espeak crates (sherpa prebuilt
  covers it — this is the whole point).
- Node: none (sherpa-onnx-node already vendored).
- System: none new (Linux: cmake/C++/ALSA as today; espeak-ng NEVER needed
  — data files only).

## Per-OS notes (from the portability analysis)

- Linux (all distros, X11+Wayland — audio is display-agnostic): no change.
- macOS: spawner sets DYLD_LIBRARY_PATH for the daemon; Rodio CoreAudio.
- Windows: no DYLD/LD games needed (DLLs beside the binding); Rodio WASAPI.
- Packaged builds: `extraResources` must include `pi-bridge/tts-synth.js`
  + `sherpa-onnx-node` + the `sherpa-onnx-<plat>-<arch>` dir + node
  runtime presence (document; Electron `main.cjs` spawns daemon via the
  sidecar, which already knows node). pi-listen removal from the TTS path
  is a follow-up (STT never used it; agent harness still needs pi-bridge).

## Tests (normative)

- Node: metadata-patch unit (protobuf append → parse back keys), recipe
  order test (values derived from a fixture .onnx.json).
- Rust: catalog gender counts (es 2M+2F, en 2M+2F), resolve sid mapping,
  daemon protocol framing (LF + `\r` strip, id correlation), playback
  queue math — all headless (no audio device: Rodio `Sink` tests gate
  stream creation behind `SMARTPC_AUDIO_LIVE=1`, default asserts queue
  math + event sequence only).
- Contract: catalog ids/entries (sharvard/daniela/kitten/davefx, gender),
  `invalid_voice` preserved, DELETE semantics preserved, PROTOCOL pin
  unchanged, speak `model` echo = composite id.
- E2E (headless-safe): daemon `ensure`+`synth` against a fixture voice?
  Too heavy — E2E covers Settings rows/badges/buttons with mocked API only.
- Manual (dev machine, required before merge): test-play each of the 8
  voices; ear-check sharvard M/F + daniela F labels; confirm no lexicon
  errors in diagnostics.

## Open risks

- Upstream URL drift (pin `v1.0.0` tag; 404 → honest `tts_failed`).
- sherpa-onnx-node major upgrades (pin 1.13.x; helper asserts slot API).
- Daemon crash mid-queue → Rust respawns once, else fails the turn loudly.
- 190 MB opt-in downloads (sharvard 76 + daniela 114) — per-row buttons
  + sizes already disclose; no prefetch.
