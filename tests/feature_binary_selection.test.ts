import { expect, test } from "bun:test";
import { resolve } from "node:path";
import { featureCommandBinary } from "./test_harness";

const decoder = new TextDecoder();
const staleTarget = resolve(import.meta.dir, "../target/test-hooks");

test("feature binary selection rejects a test-hooks-only artifact", () => {
  const build = Bun.spawnSync({
    cmd: ["cargo", "build", "--locked", "--features", "test-hooks", "--target-dir", staleTarget],
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(build.exitCode, decoder.decode(build.stderr)).toBe(0);

  const result = Bun.spawnSync({
    cmd: [featureCommandBinary(), "--json", "config", "edit"],
    env: { ...process.env, SKILLSYNC_CONFIG_DIR: resolve(staleTarget, "config") },
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(result.exitCode).toBe(1);
  expect(decoder.decode(result.stdout)).toContain("cannot run with --json");
}, { timeout: 30000 });
