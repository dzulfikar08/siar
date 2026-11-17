import { expect, test } from "bun:test";
import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const { parseArgs } = await import("../src/cli.ts");

test("CLI arguments: JSON values, flags, dashes, = form", async () => {
  const a = await parseArgs(["--targets", '["acc_1"]', "--force", "--limit", "5", "--post-id=p_1", "--content", "plain text", "--options", '{"x":{"thread":["2/"]}}']);
  expect(a).toEqual({ targets: ["acc_1"], force: true, limit: 5, post_id: "p_1", content: "plain text", options: { x: { thread: ["2/"] } } });
});

test("CLI arguments: @file and --json", async () => {
  const dir = mkdtempSync(join(tmpdir(), "siar-cli-"));
  writeFileSync(join(dir, "post.md"), "Line one\nLine two\n");
  writeFileSync(join(dir, "args.json"), JSON.stringify({ at: "2026-11-20T08:00:00+07:00", targets: ["acc_2"] }));
  const a = await parseArgs(["--json", `@${join(dir, "args.json")}`, "--content", `@${join(dir, "post.md")}`]);
  expect(a).toEqual({ at: "2026-11-20T08:00:00+07:00", targets: ["acc_2"], content: "Line one\nLine two\n" });
});

test("CLI arguments: a string that only looks like JSON stays a string", async () => {
  expect(await parseArgs(["--content", "[not json"])).toEqual({ content: "[not json" });
  await expect(parseArgs(["stray"])).rejects.toThrow("Unexpected argument");
});
