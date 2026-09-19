import { resolve } from "node:path";
import { existsSync } from "node:fs";

type Env = Record<string, string>;

const nativeName = process.platform === "win32" ? "skillsync.exe" : "skillsync";
const defaultBinary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug", nativeName);
const testHooksTarget = resolve(import.meta.dir, "../target/test-hooks");
const featureTarget = resolve(import.meta.dir, "../target/external-editor-test-hooks");
const testHooksBinary = process.env.SKILLSYNC_TEST_HOOKS_BIN ?? resolve(testHooksTarget, "release", nativeName);
const featureBinary = process.env.SKILLSYNC_EXTERNAL_EDITOR_TEST_HOOKS_BIN ?? resolve(featureTarget, "debug", nativeName);
let featureBuilt = false;

export function featureCommandBinary(): string {
  if (!featureBuilt && !existsSync(featureBinary)) {
    const result = Bun.spawnSync({ cmd: ["cargo", "build", "--features", "external-editor,test-hooks", "--target-dir", featureTarget], stdout: "pipe", stderr: "pipe" });
    if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
    featureBuilt = true;
  }
  return featureBinary;
}

let testHooksBuilt = false;

function testHooksCommandBinary(): string {
  if (!testHooksBuilt && !existsSync(testHooksBinary)) {
    const result = Bun.spawnSync({ cmd: ["cargo", "build", "--release", "--features", "test-hooks", "--target-dir", testHooksTarget], stdout: "pipe", stderr: "pipe" });
    if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
    testHooksBuilt = true;
  }
  return testHooksBinary;
}

function cleanInheritedEnv(): Env {
  return Object.fromEntries(Object.entries(process.env).filter(([key, value]) => value !== undefined && !key.startsWith("SKILLSYNC_TEST_"))) as Env;
}

export function commandBinary(extra: Env = {}): string {
  const needsHooks = Object.keys(extra).some((key) => key.startsWith("SKILLSYNC_TEST_"));
  if (!needsHooks) return defaultBinary;
  return testHooksCommandBinary();
}

export function childEnv(extra: Env = {}): Env {
  return { ...cleanInheritedEnv(), ...extra };
}
