// Pure TTS chunking: split long replies into paced server utterances.
// No Svelte runes, no browser APIs — unit-testable under plain node.
// The store owns the queue pump; this module only shapes the text.
export const SPEAK_CHUNK_LEN = 400;
/** Server budget mirror (`tts::MAX_CHARS`): 5 chunks × 400 chars. */
export const SPEAK_MAX_CHUNKS = 5;

/**
 * Server estimate mirror (`TtsManager::estimate_ms`): ~14 chars/sec +
 * 20 s engine/playback overhead, clamped. The pump adds its own +2 s
 * grace per chunk — keep the formula here so client/server can't drift
 * (change both together).
 */
export function estimateSpeakMs(text: string): number {
  const chars = Math.max([...text].length, 1);
  return Math.min(
    Math.max(Math.floor((chars * 1000) / 14 + 20000), 25000),
    240000,
  );
}

/** Strip markdown for speech: code fences are skipped, links keep text. */
export function stripMarkdown(text: string): string {
  let out = text.replace(/```[\s\S]*?(```|$)/g, " ");
  out = out.replace(/!\[([^\]]*)\]\([^)]*\)/g, "$1");
  out = out.replace(/\[([^\]]*)\]\([^)]*\)/g, "$1");
  out = out.replace(/`([^`]*)`/g, "$1");
  out = out.replace(/^[#>+\-*]\s+/gm, "");
  out = out.replace(/(\*\*|__)(.*?)\1/g, "$2");
  out = out.replace(/(^|\W)\*([^*\n]+)\*(?=\W|$)/g, "$1$2");
  out = out.replace(/(^|\W)_([^_\n]+)_(?=\W|$)/g, "$1$2");
  out = out.replace(/~~(.*?)~~/g, "$1");
  return out
    .replace(/[ \t]+/g, " ")
    .replace(/\n{3,}/g, "\n\n")
    .trim();
}

// Sentence split (es `¿? ¡!` aware via the generic terminator class),
// newlines as hard breaks. Falls back to one chunk when nothing matches.
const SENTENCE_RE = /[^.!?…\n]+[.!?…]+["»”’]?\s*|\n+/g;

/**
 * Split text into ≤5 speakable chunks of ≤`maxLen` chars each.
 * Sentence-aware with a word-wrap fallback for overlong sentences;
 * markdown is stripped first. Empty input → no chunks (caller POSTs
 * nothing).
 */
export function splitSpeak(text: string, maxLen = SPEAK_CHUNK_LEN): string[] {
  const stripped = stripMarkdown(text);
  if (!stripped) return [];
  const parts = stripped.match(SENTENCE_RE) ?? [stripped];
  const chunks: string[] = [];
  let current = "";
  const push = (s: string): boolean => {
    const t = s.trim();
    if (!t) return chunks.length >= SPEAK_MAX_CHUNKS;
    chunks.push(t);
    return chunks.length >= SPEAK_MAX_CHUNKS;
  };
  for (const raw of parts) {
    let piece = raw.trim();
    if (!piece) continue;
    // Overlong sentence: word-wrap (hard cut when no space found).
    while (piece.length > maxLen) {
      if (current && push(current)) return chunks;
      current = "";
      let cut = piece.lastIndexOf(" ", maxLen);
      if (cut <= 0) cut = maxLen;
      if (push(piece.slice(0, cut))) return chunks;
      piece = piece.slice(cut).trim();
    }
    const candidate = current ? `${current} ${piece}` : piece;
    if (candidate.length <= maxLen) {
      current = candidate;
    } else {
      if (current && push(current)) return chunks;
      current = piece;
    }
  }
  if (current) push(current);
  return chunks.slice(0, SPEAK_MAX_CHUNKS);
}
