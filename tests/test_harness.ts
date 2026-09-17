import { resolve } from "node:path";
import { existsSync } from "node:fs";

type Env = Record<string, string>;

const defaultBinary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const featureTarget = resolve(import.meta.dir, "../target/test-hooks");
const featureBinary = process.env.SKILLSYNC_TEST_HOOKS_BIN ?? resolve(featureTarget, "debug/skillsync");
let featureBuilt = false;

function cleanInheritedEnv(): Env {
  return Object.fromEntries(Object.entries(process.env).filter(([key, value]) => value !== undefined && !key.startsWith("SKILLSYNC_TEST_"))) as Env;
}

export function commandBinary(extra: Env = {}): string {
  const needsHooks = Object.keys(extra).some((key) => key.startsWith("SKILLSYNC_TEST_"));
  if (!needsHooks) return defaultBinary;
  if (!featureBuilt && !existsSync(featureBinary)) {
    const result = Bun.spawnSync({ cmd: ["cargo", "build", "--features", "test-hooks", "--target-dir", featureTarget], stdout: "pipe", stderr: "pipe" });
    if (result.exitCode !== 0) throw new Error(new TextDecoder().decode(result.stderr));
    featureBuilt = true;
  }
  return featureBinary;
}

export function childEnv(extra: Env = {}): Env {
  return { ...cleanInheritedEnv(), ...extra };
}
