import { describe, expect, test } from "vitest";
import { formatReply } from "./reply.format";

describe("formatReply", () => {
  test("single asterisks render bold, double render italics", () => {
    expect(formatReply("Esto es *importante* de verdad")).toBe(
      "<p>Esto es <strong>importante</strong> de verdad</p>",
    );
    expect(formatReply("Esto es **un matiz** menor")).toBe(
      "<p>Esto es <em>un matiz</em> menor</p>",
    );
  });

  test("dash lines render lists, blank lines split paragraphs", () => {
    expect(formatReply("- uno\n- dos\n\ncierre")).toBe(
      "<ul><li>uno</li><li>dos</li></ul><p>cierre</p>",
    );
  });

  test("single newlines render line breaks inside a paragraph", () => {
    expect(formatReply("primera\nsegunda")).toBe("<p>primera<br>segunda</p>");
  });

  test("markup is escaped before formatting", () => {
    expect(formatReply("<script>*x*</script>")).toBe(
      "<p>&lt;script&gt;<strong>x</strong>&lt;/script&gt;</p>",
    );
  });

  test("unmatched markers are left alone", () => {
    expect(formatReply("5 * 3 = 15")).toBe("<p>5 * 3 = 15</p>");
  });

  test("empty input renders nothing", () => {
    expect(formatReply(undefined)).toBe("");
    expect(formatReply("   ")).toBe("");
  });
});
