import { expect, test } from "bun:test";
import { chmod, mkdir, mkdtemp, readFile, readdir, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
const binary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const dec = new TextDecoder();
function env(root: string) { return { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library") }; }
function run(root: string, args: string[]) { return Bun.spawnSync({ cmd: [binary, ...args], env: env(root), stdout: "pipe", stderr: "pipe" }); }
async function fixture() { const root = await mkdtemp(join(tmpdir(), "skillsync-delete-")); await mkdir(join(root, "library"), { recursive: true }); return root; }
test("delete requires --yes and preserves a complete recovery snapshot", async () => {
  const root = await fixture(); const pkg = join(root, "library/demo");
  await mkdir(join(pkg, "logs"), { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\n");
  await writeFile(join(pkg, ".env"), "SECRET=x\n"); await writeFile(join(pkg, "logs/run.log"), "log\n");
  await writeFile(join(pkg, "scripts.sh"), "#!/bin/sh\n"); await chmod(join(pkg, "scripts.sh"), 0o755);
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const refused = run(root, ["--json", "delete", "demo"]); expect(refused.exitCode).toBe(1); expect(dec.decode(refused.stdout)).toContain("confirmation");
  const deleted = run(root, ["--json", "delete", "demo", "--yes"]); expect(deleted.exitCode).toBe(0);
  const result = JSON.parse(dec.decode(deleted.stdout)); expect(result.status).toBe("deleted");
  const snapshot = join(result.recovery_path, "package");
  expect(await readFile(join(snapshot, ".env"), "utf8")).toBe("SECRET=x\n"); expect(await readFile(join(snapshot, "logs/run.log"), "utf8")).toBe("log\n");
  expect((await stat(join(snapshot, "scripts.sh"))).mode & 0o111).toBe(0o111); expect(await Bun.file(pkg).exists()).toBe(false);
  const absent = run(root, ["--json", "delete", "demo", "--yes"]); expect(absent.exitCode).toBe(1); expect(JSON.parse(dec.decode(absent.stdout)).message).toContain("already_absent");
});
test("delete state-save failure restores canonical content and keeps recovery", async () => {
  const root = await fixture(); const pkg = join(root, "library/demo"); await mkdir(pkg, { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\ncontent\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const failed = Bun.spawnSync({ cmd: [binary, "--json", "delete", "demo", "--yes"], env: { ...env(root), SKILLSYNC_TEST_FAIL_STATE_SAVE: "1" }, stdout: "pipe", stderr: "pipe" });
  expect(failed.exitCode).toBe(1); expect(await readFile(join(pkg, "SKILL.md"), "utf8")).toContain("content"); expect((await readdir(join(root, "config", "recovery"))).length).toBe(1);
  expect(JSON.parse(dec.decode(run(root, ["--json", "status"]).stdout)).local_adoptions).toEqual({});
});
test("delete rejects symlinked entries without deleting canonical or external data", async () => {
  if (process.platform === "win32") return;
  const root = await fixture(); const pkg = join(root, "library/demo"); const external = join(root, "external.txt");
  await mkdir(pkg, { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\n");
  await writeFile(external, "must not be read\n"); await symlink(external, join(pkg, "linked.txt"));
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const rejected = run(root, ["--json", "delete", "demo", "--yes"]);
  expect(rejected.exitCode).toBe(1); expect(dec.decode(rejected.stdout)).toMatch(/symlink|unsupported delete snapshot entry/);
  expect(await readFile(external, "utf8")).toBe("must not be read\n");
  expect(await readFile(join(pkg, "SKILL.md"), "utf8")).toBe("name: demo\n");
});
