import { expect, test } from "bun:test";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const binary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const text = new TextDecoder();
const env = (root: string) => ({ ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library"), GIT_TERMINAL_PROMPT: "0" });
const run = (root: string, args: string[], stdout: "pipe" | "inherit" = "pipe") => Bun.spawnSync({ cmd: [binary, ...args], env: env(root), stdout, stderr: "pipe" });

test("refuses an unselected noninteractive subscribe before touching the repository", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-picker-"));
  const result = run(root, ["subscribe", "/definitely/not/a/repository"]);
  expect(result.exitCode).toBe(1);
  expect(text.decode(result.stderr)).not.toContain("git operation failed");
  expect(text.decode(result.stdout)).toContain("--skill");
});

test("JSON subscribe never emits picker output", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-picker-"));
  const result = run(root, ["--json", "subscribe", "/definitely/not/a/repository"]);
  const output = text.decode(result.stdout);
  expect(result.exitCode).toBe(1);
  expect(output).not.toContain("Found ");
  expect(() => JSON.parse(output)).not.toThrow();
});

test("duplicate display names require an exact source-relative path", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-picker-"));
  const repo = join(root, "repo");
  await mkdir(join(repo, "one"), { recursive: true });
  await mkdir(join(repo, "two"), { recursive: true });
  await writeFile(join(repo, "one/SKILL.md"), "name: duplicate\none\n");
  await writeFile(join(repo, "two/SKILL.md"), "name: duplicate\ntwo\n");
  const git = (args: string[]) => Bun.spawnSync({ cmd: ["git", ...args], cwd: repo, env: env(root), stdout: "pipe", stderr: "pipe" });
  expect(git(["init", "-q"]).exitCode).toBe(0);
  expect(git(["add", "."]).exitCode).toBe(0);
  expect(git(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "-qm", "initial"]).exitCode).toBe(0);
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const ambiguous = run(root, ["--json", "subscribe", repo, "--skill", "duplicate"]);
  expect(text.decode(ambiguous.stdout)).toContain("ambiguous");
  const exact = run(root, ["--json", "subscribe", repo, "--skill", "one"]);
  expect(exact.exitCode).toBe(0);
});
