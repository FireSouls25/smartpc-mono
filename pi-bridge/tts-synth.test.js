#!/usr/bin/env node
"use strict";
// tts-synth.test.js — hermetic unit + protocol tests (run: node tts-synth.test.js).
// No network, no audio device, no native sherpa load (the helper requires
// sherpa-onnx-node lazily inside synth only).
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawn } = require("node:child_process");

const helper = require("./tts-synth.js");

// Fixture mirroring a Piper .onnx.json (value source for the recipe).
const FIXTURE_JSON = {
  audio: { sample_rate: 22050 },
  espeak: { voice: "es" },
  num_speakers: 2,
  speaker_id_map: { M: 0, F: 1 },
};

test("recipe: metadata values derive from the fixture .onnx.json", () => {
  const meta = helper.metadataFromVoiceJson(FIXTURE_JSON);
  assert.equal(meta.sample_rate, "22050"); // audio.sample_rate
  assert.equal(meta.n_speakers, "2"); // num_speakers
  assert.equal(meta.language, "es"); // espeak.voice base
  assert.equal(meta.voice, "es"); // espeak.voice base
  assert.match(meta.comment, /piper/); // is_piper substring gate
});

test("recipe: speaker count falls back to speaker_id_map size", () => {
  const meta = helper.metadataFromVoiceJson({
    audio: { sample_rate: 16000 },
    espeak: { voice: "es" },
    speaker_id_map: { a: 0, b: 1, c: 2 },
  });
  assert.equal(meta.n_speakers, "3");
  assert.equal(meta.sample_rate, "16000");
});

test("metadata: append roundtrip — keys parse back", () => {
  const base = Buffer.from([0x08, 0x01]); // minimal proto-ish prefix
  const meta = helper.metadataFromVoiceJson(FIXTURE_JSON);
  const patched = helper.appendOnnxMetadata(base, meta);
  assert.ok(patched.length > base.length);
  assert.ok(Buffer.from(patched.subarray(0, base.length)).equals(base)); // prefix untouched
  const back = helper.readOnnxMetadata(patched);
  for (const [k, v] of Object.entries(meta)) assert.equal(back[k], v);
  assert.ok(helper.hasRequiredMetadata(patched));
  assert.ok(!helper.hasRequiredMetadata(base));
});

test("framing: strip exactly one trailing CR", () => {
  assert.equal(helper.stripLine('{"a":1}\r'), '{"a":1}');
  assert.equal(helper.stripLine('{"a":1}'), '{"a":1}');
  assert.equal(helper.stripLine("x\r\r"), "x\r"); // only one
});

test("ids: model id gate blocks traversal, allows catalog shapes", () => {
  assert.ok(helper.isSafeModelId("piper-es_ES-davefx-medium-int8"));
  assert.ok(helper.isSafeModelId("kitten-nano-en-v0_2"));
  assert.ok(helper.isSafeModelId("upstream-piper-es-sharvard#0"));
  assert.ok(!helper.isSafeModelId("../evil"));
  assert.ok(!helper.isSafeModelId("a/b"));
  assert.ok(!helper.isSafeModelId(""));
});

test("urls: pinned v1.0.0 base, https passthrough", () => {
  const u = helper.buildPiperUrl(
    "es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx",
  );
  assert.equal(
    u,
    `${helper.PIPER_BASE}/es/es_ES/sharvard/medium/es_ES-sharvard-medium.onnx`,
  );
  assert.match(u, /resolve\/v1\.0\.0\//);
  assert.equal(
    helper.buildPiperUrl("https://example.com/x.onnx"),
    "https://example.com/x.onnx",
  );
});

test("kitten: tarball URL pinned, scope guards hold", () => {
  assert.match(
    helper.KITTEN_TARBALL_URL,
    /^https:\/\/github\.com\/k2-fsa\/sherpa-onnx\/releases\/download\/tts-models\/kitten-nano-en-v0_2-fp16\.tar\.bz2$/,
  );
  assert.ok(helper.MAX_DOWNLOAD_BYTES >= 350 << 20);
  // Scope: dir basename must equal the bare model id.
  assert.doesNotThrow(() =>
    helper.assertModelScope(
      "/m/tts/upstream-piper-es-sharvard",
      "upstream-piper-es-sharvard",
    ),
  );
  assert.throws(
    () => helper.assertModelScope("/m/tts/other", "upstream-piper-es-sharvard"),
    /out of scope/,
  );
  // Wav must live inside the model dir.
  assert.doesNotThrow(() =>
    helper.assertWavScope("/m/tts/x/0-abc.wav", "/m/tts/x"),
  );
  assert.throws(
    () => helper.assertWavScope("/m/tts/o.wav", "/m/tts/x"),
    /out of scope/,
  );
  assert.throws(
    () => helper.assertWavScope("/m/tts/x/../o.wav", "/m/tts/x"),
    /out of scope/,
  );
});

test("extract: root found at top level or one down", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tts-synth-test-"));
  const nested = path.join(dir, "sub");
  fs.mkdirSync(nested, { recursive: true });
  fs.writeFileSync(path.join(nested, "voices.bin"), "x");
  assert.equal(helper.findExtractRoot(dir, ["voices.bin"]), nested);
  fs.writeFileSync(path.join(dir, "voices.bin"), "x");
  assert.equal(helper.findExtractRoot(dir, ["voices.bin"]), dir);
  assert.equal(helper.findExtractRoot(dir, ["nope.bin"]), null);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("seeds: embedded Piper phone table is the vits inventory", () => {
  const lines = helper.EMBEDDED_PIPER_TOKENS.trim().split("\n");
  assert.equal(lines.length, 152);
  assert.equal(lines[0], "_ 0");
  // "<phone> <id>" per line (the space phoneme itself starts with one).
  assert.ok(lines.every((l) => l.length > 2 && / \d+$/.test(l)));
  const ids = lines.map((l) => Number(l.split(" ").pop()));
  assert.deepEqual(ids, [...Array(152).keys()]); // dense 0..151
});

test("wav: 16-bit mono header roundtrip", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tts-synth-test-"));
  const wav = path.join(dir, "out.wav");
  const samples = new Float32Array([0, 0.5, -0.5, 1, -1]);
  helper.writeWav16(wav, samples, 22050);
  const buf = fs.readFileSync(wav);
  assert.equal(buf.subarray(0, 4).toString(), "RIFF");
  assert.equal(buf.readUInt16LE(20), 1); // PCM
  assert.equal(buf.readUInt16LE(34), 16); // 16-bit
  assert.equal(buf.readUInt32LE(24), 22050);
  assert.equal(buf.readInt16LE(44 + 3 * 2), 32767);
  fs.rmSync(dir, { recursive: true, force: true });
});

// Live protocol framing against the real stdio loop: CR strip, id echo,
// unknown-cmd error, unknown-modelId synth fails closed in one line.
test("protocol: CR strip, id echo, unknown-cmd + unknown-model errors", async () => {
  const child = spawn(
    process.execPath,
    [path.join(__dirname, "tts-synth.js")],
    {
      stdio: ["pipe", "pipe", "pipe"],
    },
  );
  const replies = [];
  let outBuf = "";
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (c) => {
    outBuf += c;
    const lines = outBuf.split("\n");
    outBuf = lines.pop();
    for (const l of lines) if (l) replies.push(JSON.parse(l));
  });
  const waitFor = async (n, ms = 5000) => {
    const t0 = Date.now();
    while (replies.length < n) {
      if (Date.now() - t0 > ms) throw new Error("protocol timeout");
      await new Promise((r) => setTimeout(r, 10));
    }
  };
  const send = (s) => child.stdin.write(s);

  send('{"id":"a1","cmd":"nope"}\r\n'); // CRLF: CR must be stripped before parse
  send(
    '{"id":"a2","cmd":"synth","modelId":"no-such-model","modelDir":"/tmp/no-such-model","sid":0,"text":"hi","outWav":"/tmp/no-such-model/o.wav"}\n',
  );
  send("not json\n");
  await waitFor(3);
  // Replies can race (bad-json answers sync, synth fails on its own
  // microtask): correlate by id, never by position.
  const byId = new Map(replies.map((r) => [r.id, r]));

  assert.equal(byId.get("a1").ok, false);
  assert.match(byId.get("a1").error, /unknown cmd/);

  assert.equal(byId.get("a2").ok, false);
  assert.match(byId.get("a2").error, /unknown tts model/);
  assert.ok(
    !byId.get("a2").error.includes("\n") && !/at\s/.test(byId.get("a2").error),
    "one line, no stack",
  );

  assert.equal(byId.get(null).ok, false); // bad json still answers (id null)

  send('{"id":"bye","cmd":"quit"}\n');
  await waitFor(4);
  assert.deepEqual(byId.get("bye") ?? replies.find((r) => r.id === "bye"), {
    id: "bye",
    ok: true,
  });
  // Race-safe exit wait: quit exits fast, possibly before we listen.
  await new Promise((res) => {
    if (child.exitCode !== null || child.signalCode !== null) res(undefined);
    else child.once("exit", () => res(undefined));
  });
  assert.equal(child.exitCode, 0);
});
