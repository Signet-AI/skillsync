#!/usr/bin/env node

import { existsSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const launcherDir = dirname(fileURLToPath(import.meta.url));
const platform = process.platform;
const architecture = process.arch;
const nativeName = platform === "win32" ? "skillsync.exe" : "skillsync";
const bundled = join(launcherDir, "..", "native", `${platform}-${architecture}`, nativeName);
const override = process.env.SKILLSYNC_NATIVE_BIN;
const nativeBinary = override || bundled;

if (!existsSync(nativeBinary)) {
  process.stderr.write(
    `skillsync launcher error: native binary not found: ${nativeBinary}\n` +
      (override
        ? "SKILLSYNC_NATIVE_BIN points to a missing file.\n"
        : "This private launcher has no bundled native artifact for this platform.\n"),
  );
  process.exitCode = 1;
} else {
  const result = spawnSync(nativeBinary, process.argv.slice(2), { stdio: "inherit" });
  if (result.error) {
    process.stderr.write(`skillsync launcher error: could not start native binary: ${result.error.message}\n`);
    process.exitCode = 1;
  } else if (result.signal) {
    process.kill(process.pid, result.signal);
  } else {
    process.exitCode = result.status ?? 1;
  }
}
