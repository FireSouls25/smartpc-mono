#!/usr/bin/env node
"use strict";
/**
 * tts-synth.js — A2 TTS synth daemon (spec: docs/13-a2-tts-engine.md).
 *
 * Zero new npm deps: node builtins + the vendored
 * `pi-bridge/node_modules/sherpa-onnx-node` (loaded lazily on first synth).
 *
 * ── Protocol: stdio, LF-delimited JSON ────────────────────────────────────
 * One request per line on stdin, one reply per line on stdout. A single
 * trailing CR per line is stripped (tolerates CRLF spawners). Every reply
 * echoes the request `id` for correlation. Logs go to stderr — stdout is
 * protocol-only.
 *
 *   ensure: download + patch a Piper voice from upstream, seed shared files
 *     {"id":1,"cmd":"ensure","modelId":"upstream-piper-es-sharvard",
 *      "modelDir":"<abs>/.pi/models/tts/<id>",
 *      "files":{"onnx":"es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx",
 *               "json":"es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx.json"},
 *      "meta":{"language":"es","voice":"es"},   // optional override; default from .onnx.json
 *      "tokensSeed":"<abs>/_shared/tokens.txt",
 *      "dataSeed":"<abs>/_shared/espeak-ng-data"}
 *     → {"id":1,"ok":true,"dir":"<modelDir>","cached":false}
 *
 *   synth: render text to a 16-bit WAV via the sherpa vits slot
 *     {"id":2,"cmd":"synth","modelId":"...","modelDir":"<abs>",
 *      "sid":0,"text":"hola","outWav":"<abs>.wav"}
 *     → {"id":2,"ok":true,"wav":"<abs>.wav","ms":812,"samples":17864}
 *     → {"id":2,"ok":false,"error":"unknown tts model: ..."}  (one line, never a stack)
 *
 *   stop: flag the in-flight synth as stopped (its reply becomes
 *     {ok:false,error:"stopped"}); idle daemon replies {ok:true,stopped:false}
 *     {"id":3,"cmd":"stop"} → {"id":3,"ok":true,"stopped":true}
 *
 *   quit: reply ok, then exit(0) after the reply flushes.
 *     {"id":4,"cmd":"quit"} → {"id":4,"ok":true}
 *
 * Unknown `cmd`, bad JSON, and unknown `modelId` all fail closed with a
 * single-line `error` string (no stack traces on the wire).
 *
 * ── Proven recipe (do not redesign) ───────────────────────────────────────
 * Upstream Piper voices load in the sherpa vits slot iff the .onnx carries
 * ONNX metadata (protobuf field-14 entries): sample_rate (from the
 * .onnx.json `audio.sample_rate`), n_speakers (from `num_speakers`),
 * language + voice (from `espeak.voice` base, e.g. `es`), and comment
 * containing the substring `piper` (sherpa sets is_piper by substring
 * match). Plus shared tokens.txt + espeak-ng-data alongside every voice,
 * and sid per speaker_id_map. Appending field-14 entries at EOF is valid
 * protobuf (fields merge in any order), so patching never rewrites the
 * model bytes. `ensure` is idempotent: present files with the required
 * metadata keys are never re-downloaded or re-patched.
 *
 * ── Upstream URLs ─────────────────────────────────────────────────────────
 * files.onnx/json are paths under
 *   https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0/<path>
 * (pinned v1.0.0 tag; 404 → honest one-line error). Full https:// URLs are
 * also accepted verbatim. Example sharvard:
 *   es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx
 *
 * ── Native library / spawner environment ──────────────────────────────────
 * The sherpa binding lives at
 *   pi-bridge/node_modules/sherpa-onnx-node/            (JS entry)
 *   pi-bridge/node_modules/sherpa-onnx-<platform>-<arch>/ (native lib dir,
 *     resolved at runtime by resolveSherpaLibDir(), e.g. sherpa-onnx-linux-x64)
 * The spawner MUST set, before spawning this daemon:
 *   Linux: export LD_LIBRARY_PATH=<lib-dir>:$LD_LIBRARY_PATH
 *   macOS: export DYLD_LIBRARY_PATH=<lib-dir>:$DYLD_LIBRARY_PATH
 *   Windows: nothing (DLLs sit beside the binding).
 * If the binding fails to load, the synth reply names the missing dir.
 *
 * ── Synth slot config ─────────────────────────────────────────────────────
 * vits { model, tokens, dataDir, noiseScale 0.667, noiseScaleW 0.8,
 * lengthScale 1.0 }, numThreads 2, provider cpu. Legacy
 * generateAsync({text, sid, speed}) shape (no onProgress: the binding
 * crashes invoking JS progress callbacks from its C++ thread).
 */

const fs = require("node:fs");
const path = require("node:path");
const os = require("node:os");

// ─── Constants ─────────────────────────────────────────────────────────────

const PIPER_BASE = "https://huggingface.co/rhasspy/piper-voices/resolve/v1.0.0";
const TTS_RELEASE =
  "https://github.com/k2-fsa/sherpa-onnx/releases/download/tts-models";
// Kitten Nano tarball: the en model AND the seed source for shared files
// (tokens.txt + espeak-ng-data) on fresh installs. Contents verified:
// model.fp16.onnx, voices.bin, tokens.txt, espeak-ng-data/ at root.
const KITTEN_TARBALL_URL = `${TTS_RELEASE}/kitten-nano-en-v0_2-fp16.tar.bz2`;
// Largest single download we ever attempt (daniela 114 MB + headroom).
// Guards the daemon against compromised/huge upstream responses.
const MAX_DOWNLOAD_BYTES = 350 << 20;
// ONNX metadata keys the vits slot requires (comment must contain "piper").
const REQUIRED_META_KEYS = [
  "sample_rate",
  "n_speakers",
  "language",
  "voice",
  "comment",
];
const MODEL_ID_RE = /^[A-Za-z0-9][A-Za-z0-9_.#-]*$/;
const MAX_TEXT_CHARS = 4000;

// ─── Small pure helpers (exported for unit tests) ──────────────────────────

/** Strip exactly one trailing CR (CRLF tolerance); LF is the delimiter. */
function stripLine(line) {
  if (line.endsWith("\r")) return line.slice(0, -1);
  return line;
}

/** Collapse to a single line for the wire: no newlines, no stack traces. */
function oneLine(err) {
  const msg =
    err && err.message !== undefined ? String(err.message) : String(err);
  return msg.split("\n")[0].replace(/\r/g, "").slice(0, 300) || "error";
}

function isSafeModelId(id) {
  return typeof id === "string" && MODEL_ID_RE.test(id) && id.length <= 128;
}

/** Upstream URL for a files entry: full https URL verbatim, else under the pinned tag. */
function buildPiperUrl(p) {
  const s = String(p || "").replace(/^\/+/, "");
  if (/^https:\/\//i.test(s)) return s;
  return `${PIPER_BASE}/${s}`;
}

function isAbs(p) {
  return typeof p === "string" && path.isAbsolute(p);
}

// ─── ONNX metadata (protobuf field 14) ─────────────────────────────────────
// Minimal encoder/decoder for ModelProto.metadata_props (field 14,
// repeated StringStringEntryProto { key = 1, value = 2 }). Appending
// entries at EOF is valid protobuf; the decoder scans top-level fields.

function encodeVarint(n) {
  const out = [];
  n = Math.floor(n);
  do {
    let b = n & 0x7f;
    n = Math.floor(n / 128);
    if (n > 0) b |= 0x80;
    out.push(b);
  } while (n > 0);
  return Buffer.from(out);
}

function readVarint(buf, pos) {
  let result = 0;
  let shift = 0;
  while (pos < buf.length) {
    const b = buf[pos++];
    result += (b & 0x7f) * 2 ** shift;
    shift += 7;
    if ((b & 0x80) === 0) return [result, pos];
    if (shift > 63) throw new Error("varint too long");
  }
  throw new Error("truncated varint");
}

function encodeEntry(key, value) {
  const kb = Buffer.from(String(key), "utf8");
  const vb = Buffer.from(String(value), "utf8");
  return Buffer.concat([
    Buffer.from([0x0a]),
    encodeVarint(kb.length),
    kb, // field 1: key
    Buffer.from([0x12]),
    encodeVarint(vb.length),
    vb, // field 2: value
  ]);
}

/** Serialize one metadata_props entry as a top-level field-14 record. */
function encodeMetadataRecord(key, value) {
  const entry = encodeEntry(key, value);
  // field 14, wire type 2 (length-delimited): tag = (14 << 3) | 2 = 114
  return Buffer.concat([Buffer.from([114]), encodeVarint(entry.length), entry]);
}

/** Append metadata entries to ONNX bytes. Returns a new Buffer. */
function appendOnnxMetadata(onnxBytes, meta) {
  const parts = [Buffer.from(onnxBytes)];
  for (const [k, v] of Object.entries(meta)) {
    parts.push(encodeMetadataRecord(k, v));
  }
  return Buffer.concat(parts);
}

function skipField(buf, pos, wireType) {
  if (wireType === 0) {
    const [, next] = readVarint(buf, pos);
    return next;
  }
  if (wireType === 1) return pos + 8;
  if (wireType === 2) {
    const [len, next] = readVarint(buf, pos);
    return next + len;
  }
  if (wireType === 5) return pos + 4;
  throw new Error(`unsupported wire type ${wireType}`);
}

function parseEntry(buf) {
  let key = "";
  let value = "";
  let pos = 0;
  while (pos < buf.length) {
    const [tag, next] = readVarint(buf, pos);
    pos = next;
    const field = tag >> 3;
    const wire = tag & 7;
    if (wire !== 2) {
      pos = skipField(buf, pos, wire);
      continue;
    }
    const [len, after] = readVarint(buf, pos);
    const s = buf.subarray(after, after + len).toString("utf8");
    pos = after + len;
    if (field === 1) key = s;
    else if (field === 2) value = s;
  }
  return [key, value];
}

/** Parse back top-level field-14 metadata entries (last value wins). */
function readOnnxMetadata(onnxBytes) {
  const buf = Buffer.from(onnxBytes);
  const meta = {};
  let pos = 0;
  while (pos < buf.length) {
    let tag;
    try {
      const [t, next] = readVarint(buf, pos);
      tag = t;
      pos = next;
    } catch {
      break; // trailing bytes that are not protobuf: stop, keep what we have
    }
    const field = tag >> 3;
    const wire = tag & 7;
    if (field === 14 && wire === 2) {
      const [len, after] = readVarint(buf, pos);
      const entryBuf = buf.subarray(after, after + len);
      pos = after + len;
      const [k, v] = parseEntry(entryBuf);
      if (k) meta[k] = v;
    } else {
      try {
        pos = skipField(buf, pos, wire);
      } catch {
        break;
      }
    }
    if (pos <= 0 || pos > buf.length) break;
  }
  return meta;
}

function hasRequiredMetadata(onnxBytes) {
  try {
    const meta = readOnnxMetadata(onnxBytes);
    return REQUIRED_META_KEYS.every(
      (k) => typeof meta[k] === "string" && meta[k].length > 0,
    );
  } catch {
    return false;
  }
}

// ─── Recipe: metadata values derived from the .onnx.json ────────────────────

/**
 * Derive the five required ONNX metadata values from a Piper .onnx.json
 * object (the json is the value source only — it never ships to sherpa).
 * Optional `override` ({language, voice}) wins for those two keys.
 */
function metadataFromVoiceJson(json, override) {
  const audio = (json && json.audio) || {};
  const espeakVoice = String(
    (json && json.espeak && json.espeak.voice) || "es",
  );
  const base = espeakVoice.split(/[-_]/)[0] || "es";
  let nSpeakers = json && json.num_speakers;
  if (!Number.isFinite(Number(nSpeakers))) {
    const map = (json && json.speaker_id_map) || {};
    nSpeakers = Object.keys(map).length || 1;
  }
  // sherpa sets is_piper by substring match on comment — always tag it.
  const comment = `piper voice ${espeakVoice}`;
  return {
    sample_rate: String(audio.sample_rate ?? 22050),
    n_speakers: String(nSpeakers),
    language: String((override && override.language) || base),
    voice: String((override && override.voice) || base),
    comment,
  };
}

// ─── Sherpa resolution (runtime, next to this helper) ───────────────────────

function sherpaPlatformArch() {
  const platform = os.platform() === "win32" ? "win" : os.platform();
  return `${platform}-${os.arch()}`;
}

/** Native lib dir sibling to pi-bridge: sherpa-onnx-<platform>-<arch>. */
function resolveSherpaLibDir() {
  return path.join(
    __dirname,
    "node_modules",
    `sherpa-onnx-${sherpaPlatformArch()}`,
  );
}

function sherpaJsEntry() {
  return path.join(
    __dirname,
    "node_modules",
    "sherpa-onnx-node",
    "sherpa-onnx.js",
  );
}

let sherpaModule = null;
function loadSherpaModule() {
  if (sherpaModule) return sherpaModule;
  const libDir = resolveSherpaLibDir();
  let mod;
  try {
    mod = require(sherpaJsEntry());
  } catch (e) {
    const hint =
      os.platform() === "darwin"
        ? `set DYLD_LIBRARY_PATH=${libDir}:$DYLD_LIBRARY_PATH`
        : os.platform() === "win32"
          ? `missing DLLs beside the binding in ${libDir}`
          : `set LD_LIBRARY_PATH=${libDir}:$LD_LIBRARY_PATH`;
    throw new Error(`sherpa load failed (${oneLine(e)}; ${hint})`, {
      cause: e,
    });
  }
  if (!mod || !mod.OfflineTts)
    throw new Error("sherpa load failed (no OfflineTts export)");
  sherpaModule = mod;
  return sherpaModule;
}

// ─── WAV (16-bit PCM mono) ──────────────────────────────────────────────────

function writeWav16(filePath, samples, sampleRate) {
  const n = samples.length;
  const data = Buffer.allocUnsafe(n * 2);
  for (let i = 0; i < n; i++) {
    const s = Math.max(-1, Math.min(1, samples[i]));
    data.writeInt16LE(Math.round(s * 32767), i * 2);
  }
  const header = Buffer.alloc(44);
  header.write("RIFF", 0);
  header.writeUInt32LE(36 + data.length, 4);
  header.write("WAVE", 8);
  header.write("fmt ", 12);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20); // PCM
  header.writeUInt16LE(1, 22); // mono
  header.writeUInt32LE(sampleRate, 24);
  header.writeUInt32LE(sampleRate * 2, 28);
  header.writeUInt16LE(2, 32);
  header.writeUInt16LE(16, 34);
  header.write("data", 36);
  header.writeUInt32LE(data.length, 40);
  fs.writeFileSync(filePath, Buffer.concat([header, data]));
}

// ─── Daemon state ────────────────────────────────────────────────────────────

// Per-model download lock: concurrent ensures for one modelId share a flight.
const inFlightEnsures = new Map(); // modelId -> Promise<{dir,cached}>
const ttsCache = new Map(); // modelDir -> OfflineTts
let synthChain = Promise.resolve();
let currentSynth = null; // { id, stopped }

async function downloadToFile(url, destTmp, maxBytes = MAX_DOWNLOAD_BYTES) {
  let res;
  try {
    res = await fetch(url, { redirect: "follow" });
  } catch (e) {
    throw new Error(`download failed: ${url} (${oneLine(e)})`, { cause: e });
  }
  if (!res.ok)
    throw new Error(`download failed: HTTP ${res.status} for ${url}`);
  const announced = Number(res.headers.get("content-length"));
  if (Number.isFinite(announced) && announced > maxBytes) {
    throw new Error(
      `download refused: ${announced} bytes exceeds the ${maxBytes} cap for ${url}`,
    );
  }
  // Stream to disk with a running cap (undici body is a WHATWG stream:
  // async-iterate it, never hold the whole body in memory).
  const out = fs.createWriteStream(destTmp);
  let total = 0;
  try {
    for await (const chunk of res.body) {
      total += chunk.length;
      if (total > maxBytes) {
        throw new Error(
          `download refused: exceeds the ${maxBytes} cap for ${url}`,
        );
      }
      if (!out.write(chunk)) {
        await new Promise((resumed) => out.once("drain", resumed));
      }
    }
  } catch (e) {
    try {
      out.destroy();
    } catch {
      /* best effort */
    }
    try {
      fs.unlinkSync(destTmp);
    } catch {
      /* already gone */
    }
    throw e;
  }
  await new Promise((resolve, reject) => {
    out.on("error", reject);
    out.end(resolve);
  }).catch((e) => {
    try {
      fs.unlinkSync(destTmp);
    } catch {
      /* already gone */
    }
    throw e;
  });
  const size = fs.statSync(destTmp).size;
  if (size === 0) {
    try {
      fs.unlinkSync(destTmp);
    } catch {
      /* already gone */
    }
    throw new Error(`download failed: empty body for ${url}`);
  }
  return size;
}

function copySeedFile(seed, dest) {
  if (fs.existsSync(dest)) return false;
  if (!fs.existsSync(seed) || !fs.statSync(seed).isFile()) {
    throw new Error(`seed tokens.txt missing: ${seed}`);
  }
  fs.copyFileSync(seed, dest);
  return true;
}

function copySeedDir(seedDir, destDir) {
  if (fs.existsSync(destDir)) return false;
  if (!fs.existsSync(seedDir) || !fs.statSync(seedDir).isDirectory()) {
    throw new Error(`seed espeak-ng-data missing: ${seedDir}`);
  }
  fs.cpSync(seedDir, destDir, { recursive: true });
  return true;
}

// Canonical espeak phone table for Piper vits voices (sourced from
// the pi-listen-packed davefx cache; the inventory is espeak-ng's
// universal set, NOT voice-specific — every espeak-phoneme Piper voice
// maps into it. Kitten's own tokens.txt is a DIFFERENT table (175 lines)
// and crashes vits synthesis with a native out_of_range, so it must
// never seed Piper voices. Written verbatim on fresh installs: fully
// offline, zero extra download.
const EMBEDDED_PIPER_TOKENS =
  "_ 0\n^ 1\n$ 2\n  3\n! 4\n' 5\n( 6\n) 7\n, 8\n- 9\n. 10\n: 11\n; 12\n? 13\na 14\nb 15\nc 16\nd 17\ne 18\nf 19\nh 20\ni 21\nj 22\nk 23\nl 24\nm 25\nn 26\no 27\np 28\nq 29\nr 30\ns 31\nt 32\nu 33\nv 34\nw 35\nx 36\ny 37\nz 38\n\u00e6 39\n\u00e7 40\n\u00f0 41\n\u00f8 42\n\u0127 43\n\u014b 44\n\u0153 45\n\u01c0 46\n\u01c1 47\n\u01c2 48\n\u01c3 49\n\u0250 50\n\u0251 51\n\u0252 52\n\u0253 53\n\u0254 54\n\u0255 55\n\u0256 56\n\u0257 57\n\u0258 58\n\u0259 59\n\u025a 60\n\u025b 61\n\u025c 62\n\u025e 63\n\u025f 64\n\u0260 65\n\u0261 66\n\u0262 67\n\u0263 68\n\u0264 69\n\u0265 70\n\u0266 71\n\u0267 72\n\u0268 73\n\u026a 74\n\u026b 75\n\u026c 76\n\u026d 77\n\u026e 78\n\u026f 79\n\u0270 80\n\u0271 81\n\u0272 82\n\u0273 83\n\u0274 84\n\u0275 85\n\u0276 86\n\u0278 87\n\u0279 88\n\u027a 89\n\u027b 90\n\u027d 91\n\u027e 92\n\u0280 93\n\u0281 94\n\u0282 95\n\u0283 96\n\u0284 97\n\u0288 98\n\u0289 99\n\u028a 100\n\u028b 101\n\u028c 102\n\u028d 103\n\u028e 104\n\u028f 105\n\u0290 106\n\u0291 107\n\u0292 108\n\u0294 109\n\u0295 110\n\u0298 111\n\u0299 112\n\u029b 113\n\u029c 114\n\u029d 115\n\u029f 116\n\u02a1 117\n\u02a2 118\n\u02b2 119\n\u02c8 120\n\u02cc 121\n\u02d0 122\n\u02d1 123\n\u02de 124\n\u03b2 125\n\u03b8 126\n\u03c7 127\n\u1d7b 128\n\u2c71 129\n0 130\n1 131\n2 132\n3 133\n4 134\n5 135\n6 136\n7 137\n8 138\n9 139\n\u0327 140\n\u0303 141\n\u032a 142\n\u032f 143\n\u0329 144\n\u02b0 145\n\u02e4 146\n\u03b5 147\n\u2193 148\n# 149\n\" 150\n\u2191 151\n";

// Kitten tarball contents (verified): model.fp16.onnx, voices.bin,
// tokens.txt, espeak-ng-data/ at the archive root.
const KITTEN_WANT = [
  "model.fp16.onnx",
  "voices.bin",
  "tokens.txt",
  "espeak-ng-data",
];

/** Extract a .tar.bz2 via the system tar (present on Linux/macOS/Win10+).
 * Fail-closed when tar is missing — never silently half-extracted. */
async function runTarExtract(archive, destDir) {
  const { execFile } = require("node:child_process");
  await new Promise((resolve, reject) => {
    execFile(
      "tar",
      ["-xjf", archive, "-C", destDir],
      (err, _stdout, stderr) => {
        if (err)
          reject(new Error(`tar extract failed (${oneLine(stderr || err)})`));
        else resolve(undefined);
      },
    );
  });
}

/** Locate the extracted tree root: files may sit at the archive root
 * or one level down (tarballs vary). Returns null when not found. */
function findExtractRoot(tmp, want) {
  if (want.every((f) => fs.existsSync(path.join(tmp, f)))) return tmp;
  let entries;
  try {
    entries = fs
      .readdirSync(tmp, { withFileTypes: true })
      .filter((e) => e.isDirectory())
      .map((e) => path.join(tmp, e.name));
  } catch {
    return null;
  }
  const dirs = entries;
  for (const d of dirs) {
    if (want.every((f) => fs.existsSync(path.join(d, f)))) return d;
  }
  return null;
}
/** Seed shared phone-table files (fresh installs).
 * tokens.txt is ALWAYS the embedded Piper/espeak table (never the kitten
 * one — different phone inventory, crashes vits synthesis). espeak-ng-data
 * comes from the kitten tarball (structurally identical set, verified).
 * No-op when both targets already exist. */
async function ensureSharedSeeds(tokensPath, dataDir) {
  if (fs.existsSync(tokensPath) && fs.existsSync(dataDir)) return false;
  if (!fs.existsSync(tokensPath)) {
    fs.mkdirSync(path.dirname(tokensPath), { recursive: true });
    fs.writeFileSync(tokensPath, EMBEDDED_PIPER_TOKENS, "utf8");
  }
  if (!fs.existsSync(dataDir)) {
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "tts-seed-"));
    try {
      const archive = path.join(tmp, "kitten.tar.bz2");
      await downloadToFile(KITTEN_TARBALL_URL, archive);
      const out = path.join(tmp, "x");
      fs.mkdirSync(out, { recursive: true });
      await runTarExtract(archive, out);
      const root = findExtractRoot(out, KITTEN_WANT);
      if (!root) throw new Error("kitten tarball missing expected files");
      fs.mkdirSync(path.dirname(dataDir), { recursive: true });
      fs.cpSync(path.join(root, "espeak-ng-data"), dataDir, {
        recursive: true,
      });
    } finally {
      fs.rmSync(tmp, { recursive: true, force: true });
    }
  }
  return true;
}

/** Defense-in-depth path containment (the HTTP surface never passes paths,
 * but a future caller might): modelDir must be named exactly the bare
 * model id, and outWav must live inside modelDir. */
function assertModelScope(modelDir, modelId) {
  if (path.basename(modelDir) !== modelId) {
    throw new Error(`model dir out of scope: ${path.basename(modelDir)}`);
  }
}

function assertWavScope(outWav, modelDir) {
  const rel = path.relative(modelDir, outWav);
  if (!rel || rel.startsWith("..") || path.isAbsolute(rel)) {
    throw new Error("wav path out of scope");
  }
}

async function doEnsure(req) {
  const { modelId, modelDir, files, meta, tokensSeed, dataSeed } = req;
  if (!isSafeModelId(modelId))
    throw new Error(`bad model id: ${String(modelId).slice(0, 60)}`);
  if (!isAbs(modelDir)) throw new Error("modelDir must be an absolute path");
  assertModelScope(modelDir, modelId);
  if (
    typeof tokensSeed !== "string" ||
    !tokensSeed ||
    typeof dataSeed !== "string" ||
    !dataSeed
  ) {
    throw new Error("tokensSeed and dataSeed paths are required");
  }

  fs.mkdirSync(modelDir, { recursive: true });

  // Shared phone-table files first: fresh installs seed them from the
  // kitten tarball (also the en model — one download, two purposes).
  await ensureSharedSeeds(tokensSeed, dataSeed);

  // Kitten slot: whole-tarball model (voices.bin + fp16 onnx + own
  // tokens/data inside the model dir; shared seeds stay for vits).
  if (req.kitten === true) {
    const voices = path.join(modelDir, "voices.bin");
    const fp16 = path.join(modelDir, "model.fp16.onnx");
    const flat = path.join(modelDir, "model.onnx");
    const ready =
      (fs.existsSync(fp16) || fs.existsSync(flat)) && fs.existsSync(voices);
    if (ready) return { dir: modelDir, cached: true };
    const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "tts-kitten-"));
    try {
      const archive = path.join(tmp, "kitten.tar.bz2");
      await downloadToFile(KITTEN_TARBALL_URL, archive);
      await runTarExtract(archive, tmp);
      const root = findExtractRoot(tmp, KITTEN_WANT);
      if (!root) throw new Error("kitten tarball missing expected files");
      for (const f of KITTEN_WANT) {
        const from = path.join(root, f);
        const to = path.join(modelDir, f);
        if (fs.existsSync(to)) continue;
        if (fs.statSync(from).isDirectory())
          fs.cpSync(from, to, { recursive: true });
        else fs.copyFileSync(from, to);
      }
    } finally {
      fs.rmSync(tmp, { recursive: true, force: true });
    }
    return { dir: modelDir, cached: false };
  }

  if (
    !files ||
    typeof files.onnx !== "string" ||
    !files.onnx ||
    typeof files.json !== "string" ||
    !files.json
  ) {
    throw new Error("files.onnx and files.json (upstream paths) are required");
  }
  const onnxPath = path.join(modelDir, "model.onnx");
  const jsonPath = path.join(modelDir, "model.onnx.json");
  const tokensPath = path.join(modelDir, "tokens.txt");
  const dataDir = path.join(modelDir, "espeak-ng-data");

  const ready =
    fs.existsSync(onnxPath) &&
    fs.existsSync(jsonPath) &&
    fs.existsSync(tokensPath) &&
    fs.existsSync(dataDir) &&
    hasRequiredMetadata(fs.readFileSync(onnxPath));
  if (ready) return { dir: modelDir, cached: true };

  // Download whichever of the pair is missing (json first: it is the
  // metadata value source for the patch step).
  if (!fs.existsSync(jsonPath)) {
    const tmp = `${jsonPath}.partial-${process.pid}`;
    try {
      await downloadToFile(buildPiperUrl(files.json), tmp);
      fs.renameSync(tmp, jsonPath);
    } finally {
      try {
        fs.unlinkSync(tmp);
      } catch {
        /* already gone */
      }
    }
  }
  let onnxFresh = false;
  if (!fs.existsSync(onnxPath)) {
    const tmp = `${onnxPath}.partial-${process.pid}`;
    try {
      await downloadToFile(buildPiperUrl(files.onnx), tmp);
      fs.renameSync(tmp, onnxPath);
    } finally {
      try {
        fs.unlinkSync(tmp);
      } catch {
        /* already gone */
      }
    }
    onnxFresh = true;
  }

  // Patch: append the recipe metadata unless the keys already parse back
  // (native-metadata caches like davefx are left byte-identical).
  if (!hasRequiredMetadata(fs.readFileSync(onnxPath))) {
    let voiceJson;
    try {
      voiceJson = JSON.parse(fs.readFileSync(jsonPath, "utf8"));
    } catch (e) {
      throw new Error(`voice json unreadable: ${oneLine(e)}`, { cause: e });
    }
    const metaValues = metadataFromVoiceJson(voiceJson, meta);
    const patched = appendOnnxMetadata(fs.readFileSync(onnxPath), metaValues);
    const tmp = `${onnxPath}.patch-${process.pid}`;
    try {
      fs.writeFileSync(tmp, patched);
      fs.renameSync(tmp, onnxPath);
    } finally {
      try {
        fs.unlinkSync(tmp);
      } catch {
        /* already gone */
      }
    }
    void onnxFresh;
  }

  // Shared files: copy once from caller-provided seeds (never hardcoded).
  copySeedFile(tokensSeed, tokensPath);
  copySeedDir(dataSeed, dataDir);

  return { dir: modelDir, cached: false };
}

function ensureFlight(req) {
  const key = req.modelId;
  const existing = inFlightEnsures.get(key);
  if (existing) return existing;
  const pending = doEnsure(req).finally(() => {
    if (inFlightEnsures.get(key) === pending) inFlightEnsures.delete(key);
  });
  inFlightEnsures.set(key, pending);
  return pending;
}

function getOrCreateTtsSync(modelDir) {
  const hit = ttsCache.get(modelDir);
  if (hit) return hit;
  const sherpa = loadSherpaModule();
  const dataDir = path.join(modelDir, "espeak-ng-data");
  const tokens = path.join(modelDir, "tokens.txt");
  // Kitten slot auto-detect: voices.bin marks a kitten model dir.
  // Upstream Piper (.onnx + shared table) stays on the vits slot.
  const kittenOnnx = ["model.fp16.onnx", "model.onnx"]
    .map((f) => path.join(modelDir, f))
    .find((p) => fs.existsSync(p));
  const slot =
    fs.existsSync(path.join(modelDir, "voices.bin")) && kittenOnnx
      ? {
          kitten: {
            model: kittenOnnx,
            voices: path.join(modelDir, "voices.bin"),
            tokens,
            dataDir,
            lengthScale: 1.0,
          },
        }
      : {
          vits: {
            model: path.join(modelDir, "model.onnx"),
            tokens,
            dataDir,
            noiseScale: 0.667,
            noiseScaleW: 0.8,
            lengthScale: 1.0,
          },
        };
  const config = { model: slot, numThreads: 2, provider: "cpu" };
  const tts = new sherpa.OfflineTts(config);
  ttsCache.set(modelDir, tts);
  return tts;
}

async function doSynth(req) {
  const { modelId, modelDir, sid, text, outWav } = req;
  if (!isSafeModelId(modelId))
    throw new Error(`unknown tts model: ${String(modelId).slice(0, 60)}`);
  if (!isAbs(modelDir)) throw new Error("modelDir must be an absolute path");
  if (!isAbs(outWav)) throw new Error("outWav must be an absolute path");
  assertModelScope(modelDir, modelId);
  assertWavScope(outWav, modelDir);
  const sidNum = Number(sid ?? 0);
  if (!Number.isFinite(sidNum) || Math.floor(sidNum) < 0)
    throw new Error(`bad sid: ${String(sid).slice(0, 20)}`);
  const clean = String(text ?? "")
    .split(/\s+/)
    .filter(Boolean)
    .join(" ");
  if (!clean) throw new Error("nothing to speak");
  const utterance = clean.slice(0, MAX_TEXT_CHARS);

  // Fail closed before touching the native engine: unknown or
  // not-yet-ensured model ids are one-line errors, never stacks.
  // (Kitten slot: model.fp16.onnx + voices.bin instead of model.onnx.)
  const onnxPath = path.join(modelDir, "model.onnx");
  const kittenReady =
    fs.existsSync(path.join(modelDir, "voices.bin")) &&
    (fs.existsSync(path.join(modelDir, "model.fp16.onnx")) ||
      fs.existsSync(onnxPath));
  if (!fs.existsSync(onnxPath) && !kittenReady)
    throw new Error(`unknown tts model: ${modelId}`);
  if (!fs.existsSync(path.join(modelDir, "tokens.txt"))) {
    throw new Error(`tts model not ready: ${modelId}`);
  }

  const started = Date.now();
  const me = { stopped: false };
  currentSynth = me;
  try {
    const tts = getOrCreateTtsSync(modelDir);
    // Legacy shape: no onProgress (the binding crashes on JS callbacks
    // from its C++ thread). Serialized per daemon via synthChain in the
    // caller so stop targets exactly this run.
    const audio = await tts.generateAsync({
      text: utterance,
      sid: Math.floor(sidNum),
      speed: 1.0,
    });
    if (me.stopped) throw new Error("stopped");
    const samples = audio && audio.samples;
    if (!samples || samples.length === 0)
      throw new Error(`synth returned no audio for ${modelId}`);
    if (Number.isNaN(samples[0]))
      throw new Error(`synth returned NaN samples for ${modelId}`);
    const sampleRate = audio.sampleRate || tts.sampleRate || 22050;
    fs.mkdirSync(path.dirname(outWav), { recursive: true });
    writeWav16(outWav, samples, sampleRate);
    return { wav: outWav, ms: Date.now() - started, samples: samples.length };
  } finally {
    if (currentSynth === me) currentSynth = null;
  }
}

// ─── Protocol loop ───────────────────────────────────────────────────────────

function reply(id, obj) {
  process.stdout.write(`${JSON.stringify({ id, ...obj })}\n`);
}

async function handleRequest(req) {
  const id =
    req && Object.prototype.hasOwnProperty.call(req, "id") ? req.id : null;
  try {
    if (!req || typeof req !== "object")
      throw new Error("bad request: object expected");
    switch (req.cmd) {
      case "ensure": {
        const result = await ensureFlight(req);
        reply(id, { ok: true, dir: result.dir, cached: result.cached });
        break;
      }
      case "synth": {
        // Serialize synths: stop/correlation stay exact with one engine.
        const run = synthChain.catch(() => {}).then(() => doSynth(req));
        synthChain = run.catch(() => {});
        const result = await run;
        reply(id, {
          ok: true,
          wav: result.wav,
          ms: result.ms,
          samples: result.samples,
        });
        break;
      }
      case "stop": {
        const stopped = currentSynth
          ? ((currentSynth.stopped = true), true)
          : false;
        reply(id, { ok: true, stopped });
        break;
      }
      case "quit": {
        reply(id, { ok: true });
        await new Promise((res) => process.stdout.write("", res));
        process.exit(0);
        break;
      }
      default:
        reply(id, {
          ok: false,
          error: `unknown cmd: ${String(req.cmd).slice(0, 40)}`,
        });
    }
  } catch (e) {
    reply(id, { ok: false, error: oneLine(e) });
  }
}

function startDaemon() {
  let buf = "";
  process.stdin.setEncoding("utf8");
  process.stdin.on("data", (chunk) => {
    buf += chunk;
    const lines = buf.split("\n");
    buf = lines.pop();
    for (const raw of lines) {
      const line = stripLine(raw);
      if (!line.trim()) continue;
      let req;
      try {
        req = JSON.parse(line);
      } catch {
        reply(null, { ok: false, error: "bad json: one object per line" });
        continue;
      }
      void handleRequest(req);
    }
  });
  process.stdin.on("end", () => process.exit(0));
  process.stdin.resume();
}

module.exports = {
  stripLine,
  oneLine,
  isSafeModelId,
  buildPiperUrl,
  encodeMetadataRecord,
  appendOnnxMetadata,
  readOnnxMetadata,
  hasRequiredMetadata,
  metadataFromVoiceJson,
  resolveSherpaLibDir,
  writeWav16,
  findExtractRoot,
  assertModelScope,
  assertWavScope,
  EMBEDDED_PIPER_TOKENS,
  PIPER_BASE,
  TTS_RELEASE,
  KITTEN_TARBALL_URL,
  MAX_DOWNLOAD_BYTES,
  REQUIRED_META_KEYS,
};

if (require.main === module) startDaemon();
