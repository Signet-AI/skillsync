import { expect, test } from "bun:test";
import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const quote = (s: string) => `'${s.replaceAll("'", "'\\''")}'`;

test("TUI panic restores terminal state and keeps the original failure visible", async () => {
  if (process.platform === "win32") return;
  const root = await mkdtemp(join(tmpdir(), "skillsync-tui-failure-"));
  const targetDir = await mkdtemp(join(tmpdir(), "skillsync-tui-target-"));
  const binary = join(targetDir, "debug", "skillsync");
  try {
    const build = Bun.spawnSync({
      cmd: ["cargo", "build", "--features", "test-hooks", "--target-dir", targetDir],
      cwd: resolve(import.meta.dir, ".."), stdout: "pipe", stderr: "pipe",
    });
    expect(build.exitCode).toBe(0);
    expect(build.stderr.toString()).not.toContain("error:");
    const config = join(root, "config"); const library = join(root, "library");
    await mkdir(library, { recursive: true });
    await writeFile(join(library, "SKILL.md"), "name: demo\n");
    const env = { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library };
    const init = Bun.spawnSync({ cmd: [binary, "--json", "init"], env, stdout: "pipe", stderr: "pipe" });
    expect(init.exitCode).toBe(0);
    const command = `before=$(stty -g); printf 'BEFORE:%s\\n' "$before"; ${quote(binary)}; status=$?; after=$(stty -g); printf 'AFTER:%s\\n' "$after"; printf 'STATUS:%s\\n' "$status"; exit 0`;
    const scriptCommand = process.platform === "darwin"
      ? ["python3", resolve(import.meta.dir, "pty_runner.py"), "sh", "-c", command]
      : ["script", "-qefc", command, "/dev/null"];
    if (process.platform === "darwin") {
      const child = Bun.spawn({
        cmd: scriptCommand,
        env: { ...env, SKILLSYNC_TEST_TUI_PANIC: "1", SKILLSYNC_PTY_KEYS: "" },
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
      });
      const outputPromise = Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
      expect(await child.exited).toBe(0);
      const [stdout, stderr] = await outputPromise;
      const output = stdout + stderr;
      const before = output.match(/BEFORE:([^\\r\\n]+)/)?.[1];
      const after = output.match(/AFTER:([^\\r\\n]+)/)?.[1];
      expect(before).toBeDefined(); expect(after).toBe(before);
      expect(output).toContain("STATUS:101");
      expect(output).toContain("injected TUI panic");
      return;
    }
    const child = Bun.spawn({ cmd: scriptCommand, env: { ...env, SKILLSYNC_TEST_TUI_PANIC: "1" }, stdin: "pipe", stdout: "pipe", stderr: "pipe" });
    const outputPromise = Promise.all([new Response(child.stdout).text(), new Response(child.stderr).text()]);
    await Bun.sleep(300); child.stdin.write("q"); child.stdin.end();
    expect(await child.exited).toBe(0);
    const [stdout, stderr] = await outputPromise;
    const output = stdout + stderr;
    const before = output.match(/BEFORE:([^\r\n]+)/)?.[1];
    const after = output.match(/AFTER:([^\r\n]+)/)?.[1];
    expect(before).toBeDefined(); expect(after).toBe(before);
    expect(output).toContain("STATUS:101");
    expect(output).toContain("injected TUI panic");
  } finally {
    await Promise.all([rm(root, { recursive: true, force: true }), rm(targetDir, { recursive: true, force: true })]);
  }
}, { timeout: 60000 });
