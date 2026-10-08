// Reply formatter for assistant messages (see the REPLY STYLE prompt
// section in `src/native/src/harness/prompt.rs`).
//
// House convention (inverted vs standard markdown, on purpose):
// single `*bold*` renders bold, double `**italic**` renders italics,
// `- ` lines render lists, blank lines split paragraphs, single
// newlines render line breaks. Everything is HTML-escaped first, so
// `{@html formatReply(...)}` is safe by construction.

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function inline(s: string): string {
  const out = escapeHtml(s)
    .replace(/\*\*(.+?)\*\*/g, "<em>$1</em>")
    .replace(/\*(.+?)\*/g, "<strong>$1</strong>");
  return out;
}

export function formatReply(raw: string | undefined): string {
  if (!raw) return "";
  const parts: string[] = [];
  for (const block of raw.split(/\n\s*\n/)) {
    const lines = block
      .split("\n")
      .map((l) => l.trim())
      .filter((l) => l.length > 0);
    if (lines.length === 0) continue;
    if (lines.every((l) => l.startsWith("- "))) {
      parts.push(
        `<ul>${lines.map((l) => `<li>${inline(l.slice(2).trim())}</li>`).join("")}</ul>`,
      );
    } else {
      parts.push(`<p>${lines.map(inline).join("<br>")}</p>`);
    }
  }
  return parts.join("");
}
