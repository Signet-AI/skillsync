import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const binary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const quote = (s: string) => `'${s.replaceAll("'", "'\\''")}'`;

test("no-arg TTY opens read-only library browser and quits without mutation", async () => {
  if (process.platform === "win32") return;
  const root = await mkdtemp(join(tmpdir(), "skillsync-tui-"));
  try {
    const config = join(root, "config"); const library = join(root, "library");
    await mkdir(join(library, "root-skill"), { recursive: true });
    await mkdir(join(library, "parent", "nested"), { recursive: true });
    await writeFile(join(library, "root-skill", "SKILL.md"), "name: root-skill\n");
    await writeFile(join(library, "parent", "SKILL.md"), "name: parent\n");
    await writeFile(join(library, "parent", "nested", "SKILL.md"), "name: nested\n");
    const env = { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library };
    const init = Bun.spawnSync({ cmd: [binary, "--json", "init"], env, stdout: "pipe", stderr: "pipe" });
    expect(init.exitCode).toBe(0);
    const before = new Map<string, string>();
    for (const name of ["state.json", "state.lock"]) before.set(name, await readFile(join(config, name), "utf8").catch(() => ""));
    before.set("library", JSON.stringify((await readdir(library, { recursive: true })).sort()));
    const command = `stty rows 30 cols 120; exec ${[binary].map(quote).join(" ")}`;
    const scriptCommand = process.platform === "darwin"
      ? ["python3", resolve(import.meta.dir, "pty_runner.py"), "sh", "-c", command]
      : ["script", "-qefc", command, "/dev/null"];
    const child = Bun.spawn({ cmd: scriptCommand, env: { ...env, SKILLSYNC_PTY_KEYS: "rq" }, stdin: "pipe", stdout: "pipe", stderr: "pipe" });
    const outputPromise = new Response(child.stdout).text();
    await Bun.sleep(300);
    child.stdin.write("\r"); await Bun.sleep(50); child.stdin.write("q"); child.stdin.end();
    const exitCode = await child.exited;
    const output = await outputPromise;
    expect(exitCode).toBe(0);
    expect(output).toContain("Library"); expect(output).toContain("root-skill"); expect(output).toContain("parent/nested"); expect(output).toContain("Provenance");
    expect(await readFile(join(config, "state.json"), "utf8")).toBe(before.get("state.json") ?? "");
    expect(await readFile(join(config, "state.lock"), "utf8").catch(() => "")).toBe(before.get("state.lock") ?? "");
    expect(JSON.stringify((await readdir(library, { recursive: true })).sort())).toBe(before.get("library") ?? "");
  } finally {
    await rm(root, { recursive: true, force: true });
  }
});
