import { describe, expect, test } from "vitest";
import {
  estimateSpeakMs,
  splitSpeak,
  stripMarkdown,
  SPEAK_CHUNK_LEN,
  SPEAK_MAX_CHUNKS,
} from "./voice.chunks";

describe("stripMarkdown", () => {
  test("skips fenced code, keeps link text", () => {
    expect(stripMarkdown("```js\nspeak(no)\n```\nHi")).toBe("Hi");
    expect(stripMarkdown("see [docs](https://x.test/y?q=1)")).toBe("see docs");
    expect(stripMarkdown("**bold** and `code`")).toBe("bold and code");
    expect(stripMarkdown("# Title\n> quote")).toBe("Title\nquote");
  });

  test("blank input stays blank", () => {
    expect(stripMarkdown("   \n ")).toBe("");
  });
});

describe("splitSpeak", () => {
  test("empty split → no chunks (caller POSTs nothing)", () => {
    expect(splitSpeak("")).toEqual([]);
    expect(splitSpeak("   ")).toEqual([]);
    expect(splitSpeak("```x```")).toEqual([]);
  });

  test("short text is one chunk", () => {
    expect(splitSpeak("Hola, ¿qué hacemos hoy?")).toEqual([
      "Hola, ¿qué hacemos hoy?",
    ]);
  });

  test("splits on sentence boundaries (es punctuation aware)", () => {
    // Short sentences pack into one chunk; long ones split at boundaries.
    expect(splitSpeak("¿Vienes? ¡Claro!")).toEqual(["¿Vienes? ¡Claro!"]);
    const para =
      "¿Vienes mañana a la reunión del equipo? ¡Claro que sí, allí estaré! " +
      "Voy a traer los informes de la semana pasada. Trae pan de camino. ".repeat(
        8,
      );
    const chunks = splitSpeak(para);
    expect(chunks.length).toBeGreaterThan(1);
    expect(chunks[0]).toContain("¿Vienes");
  });

  test("packs small sentences, respects the 400-char cap per chunk", () => {
    const sentence = "Esta es una frase corta. ";
    const chunks = splitSpeak(sentence.repeat(40));
    expect(chunks.length).toBeGreaterThan(1);
    for (const c of chunks)
      expect(c.length).toBeLessThanOrEqual(SPEAK_CHUNK_LEN);
  });

  test("word-wraps sentences longer than maxLen", () => {
    const long = "palabra ".repeat(200).trim();
    const chunks = splitSpeak(long, 400);
    expect(chunks.length).toBeGreaterThan(1);
    for (const c of chunks) expect(c.length).toBeLessThanOrEqual(400);
    expect(chunks.join(" ").replace(/\s+/g, " ")).toBe(long);
  });

  test("caps at 5 chunks (~2000-char MAX_CHARS budget)", () => {
    expect(SPEAK_MAX_CHUNKS).toBe(5);
    const chunks = splitSpeak("Frase completa número uno. ".repeat(200));
    expect(chunks).toHaveLength(5);
  });
});

describe("estimateSpeakMs", () => {
  test("mirrors the server formula (~14 chars/sec + 20 s, clamped)", () => {
    // 1400 chars ≈ 100 s + 20 s overhead.
    expect(estimateSpeakMs("x".repeat(1400))).toBe(120000);
    expect(estimateSpeakMs("")).toBeGreaterThanOrEqual(25000);
    expect(estimateSpeakMs("x".repeat(20000))).toBe(240000);
  });
});
