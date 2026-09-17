import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const binary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const decoder = new TextDecoder();

type Fixture = {
  root: string;
  config: string;
  library: string;
  env: Record<string, string>;
};

async function fixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-macos-"));
  const config = join(root, "config");
  const library = join(root, "library");
  await mkdir(join(root, "home"), { recursive: true });
  return {
    root,
    config,
    library,
    env: {
      ...process.env as Record<string, string>,
      HOME: join(root, "home"),
      SKILLSYNC_CONFIG_DIR: config,
      SKILLSYNC_LIBRARY: library,
    },
  };
}

function run(f: Fixture, args: string[]) {
  const result = Bun.spawnSync({ cmd: [binary, ...args], env: f.env, stdout: "pipe", stderr: "pipe" });
  const output = decoder.decode(result.stdout);
  expect(result.exitCode, decoder.decode(result.stderr)).toBe(0);
  expect(output).not.toBe("");
  return JSON.parse(output) as Record<string, unknown>;
}

test("supports macOS temp paths through import, delete, and restore", async () => {
  if (process.platform !== "darwin") return;
  const f = await fixture();
  try {
    const source = join(f.root, "source");
    await mkdir(join(source, "nested"), { recursive: true });
    await writeFile(join(source, "SKILL.md"), "name: demo\nmacOS\n");
    await writeFile(join(source, "nested", "run.sh"), "#!/bin/sh\n");
    await chmod(join(source, "nested", "run.sh"), 0o755);

    run(f, ["--json", "init"]);
    const imported = run(f, ["--json", "import", "--from", source, "--skill", "demo"]);
    expect(imported.status).toBe("adopted");
    expect(await readFile(join(f.library, "demo", "SKILL.md"), "utf8")).toContain("macOS");

    const deleted = run(f, ["--json", "delete", "demo", "--yes"]);
    expect(deleted.status).toBe("deleted");
    const recoveryPath = String(deleted.recovery_path);
    expect(await readFile(join(recoveryPath, "package", "SKILL.md"), "utf8")).toContain("macOS");

    const restored = run(f, ["--json", "restore", "--from", recoveryPath]);
    expect(restored.status).toBe("restored");
    expect(await readFile(join(f.library, "demo", "SKILL.md"), "utf8")).toContain("macOS");
  } finally {
    await rm(f.root, { recursive: true, force: true });
  }
});
