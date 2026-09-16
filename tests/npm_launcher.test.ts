import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const launcher = resolve(import.meta.dir, "../npm/bin/skillsync.mjs");
const decoder = new TextDecoder();
const baseEnv = () => ({ ...process.env, SKILLSYNC_NATIVE_BIN: undefined });

function run(args: string[], options: { env?: Record<string, string | undefined>; input?: string } = {}) {
  return Bun.spawnSync({
    cmd: [process.execPath, launcher, ...args],
    env: { ...baseEnv(), ...options.env },
    stdin: options.input === undefined ? "inherit" : new TextEncoder().encode(options.input),
    stdout: "pipe",
    stderr: "pipe",
  });
}

test("forwards arguments, stdin, stdout, and stderr through the override", async () => {
  if (process.platform === "win32") return;
  const root = await mkdtemp(join(tmpdir(), "skillsync-launcher-"));
  const argsFile = join(root, "args");
  const script = join(root, "native");
  await writeFile(script, `#!/bin/sh\nprintf '%s\\n' "$@" > "$ARGS_FILE"\nprintf 'native stdout\\n'\nprintf 'native stderr\\n' >&2\ncat\n`);
  await chmod(script, 0o755);
  const result = run(["--json", "arg with spaces", "", "--", "tail"], {
    env: { SKILLSYNC_NATIVE_BIN: script, ARGS_FILE: argsFile },
    input: "forwarded stdin\n",
  });
  expect(result.exitCode).toBe(0);
  expect(decoder.decode(result.stdout)).toBe("native stdout\nforwarded stdin\n");
  expect(decoder.decode(result.stderr)).toBe("native stderr\n");
  expect(await readFile(argsFile, "utf8")).toBe("--json\narg with spaces\n\n--\ntail\n");
});

test("forwards the native exit code", async () => {
  if (process.platform === "win32") return;
  const root = await mkdtemp(join(tmpdir(), "skillsync-launcher-"));
  const script = join(root, "native");
  await writeFile(script, "#!/bin/sh\nexit 37\n");
  await chmod(script, 0o755);
  expect(run([], { env: { SKILLSYNC_NATIVE_BIN: script } }).exitCode).toBe(37);
});

test("fails stably without a binary and never searches PATH", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-launcher-"));
  const pathBin = join(root, "path-bin");
  const fake = join(pathBin, process.platform === "win32" ? "skillsync.cmd" : "skillsync");
  await mkdir(pathBin, { recursive: true });
  await writeFile(fake, "@echo PATH fallback\n");
  if (process.platform !== "win32") await chmod(fake, 0o755);
  const result = run(["--json"], { env: { PATH: pathBin } });
  expect(result.exitCode).toBe(1);
  expect(decoder.decode(result.stderr)).toContain("skillsync launcher error: native binary not found:");
  expect(decoder.decode(result.stdout)).toBe("");
});

test("reports an override spawn error clearly", async () => {
  const result = run([], { env: { SKILLSYNC_NATIVE_BIN: join(tmpdir(), "does-not-exist") } });
  expect(result.exitCode).toBe(1);
  expect(decoder.decode(result.stderr)).toContain("native binary not found");
});
