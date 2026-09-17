import { expect, test } from "bun:test";
import { chmod, lstat, mkdir, mkdtemp, readFile, readdir, readlink, stat, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { childEnv, commandBinary } from "./test_harness";
const binary = commandBinary();
const dec = new TextDecoder();
function env(root: string) { return { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library") }; }
function run(root: string, args: string[], extra: Record<string, string> = {}) { return Bun.spawnSync({ cmd: [commandBinary(extra), ...args], env: childEnv({ ...env(root), ...extra }), stdout: "pipe", stderr: "pipe" }); }
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
  const failed = Bun.spawnSync({ cmd: [commandBinary({ SKILLSYNC_TEST_FAIL_STATE_SAVE: "1" }), "--json", "delete", "demo", "--yes"], env: childEnv({ ...env(root), SKILLSYNC_TEST_FAIL_STATE_SAVE: "1" }), stdout: "pipe", stderr: "pipe" });
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

test("delete fails closed when quarantine is replaced by a dangling symlink", async () => {
  if (process.platform === "win32") return;
  const root = await fixture(); const pkg = join(root, "library/demo");
  await mkdir(pkg, { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const failed = run(root, ["--json", "delete", "demo", "--yes"], { SKILLSYNC_TEST_DANGLING_QUARANTINE_SYMLINK: "1" });
  expect(failed.exitCode).toBe(1);
  expect(dec.decode(failed.stdout)).toMatch(/symlink|regular directory|cleanup target/);
  const recoveryEntries = await readdir(join(root, "config/recovery"));
  expect(recoveryEntries).toHaveLength(1);
  const recoveryPath = join(root, "config/recovery", recoveryEntries[0]!);
  const quarantine = join(recoveryPath, "quarantine");
  expect((await lstat(quarantine)).isSymbolicLink()).toBe(true);
  expect(await readlink(quarantine)).toBe("missing-quarantine-target");
  expect(await Bun.file(pkg).exists()).toBe(false);
});

test("restore rehydrates deletion snapshot and is idempotent", async () => {
  const root = await fixture(); const pkg = join(root, "library/demo");
  await mkdir(join(pkg, "logs"), { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\n");
  await writeFile(join(pkg, ".env"), "SECRET=x\n"); await writeFile(join(pkg, "logs/run.log"), "log\n");
  await writeFile(join(pkg, "scripts.sh"), "#!/bin/sh\n"); await chmod(join(pkg, "scripts.sh"), 0o755);
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const deleted = JSON.parse(dec.decode(run(root, ["--json", "delete", "demo", "--yes"]).stdout));
  const restored = run(root, ["--json", "restore", "--from", deleted.recovery_path]);
  expect(restored.exitCode).toBe(0); expect(JSON.parse(dec.decode(restored.stdout)).status).toBe("restored");
  expect(await readFile(join(pkg, ".env"), "utf8")).toBe("SECRET=x\n"); expect((await stat(join(pkg, "scripts.sh"))).mode & 0o111).toBe(0o111);
  const rerun = run(root, ["--json", "restore", "--from", deleted.recovery_path]);
  expect(JSON.parse(dec.decode(rerun.stdout)).status).toBe("already_present");
});

test("restore uses the shared state lock", async () => {
  if (process.platform !== "linux") return;
  const root = await fixture();
  const pkg = join(root, "library/demo");
  await mkdir(pkg, { recursive: true });
  await writeFile(join(pkg, "SKILL.md"), "name: demo\\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const deleted = JSON.parse(dec.decode(run(root, ["--json", "delete", "demo", "--yes"]).stdout));
  const marker = join(root, "lock-held");
  const lock = join(root, "config/state.lock");
  const holder = Bun.spawn(["flock", lock, "-c", `touch ${marker}; sleep 3`], {
    stdout: "ignore",
    stderr: "pipe",
  });
  try {
    for (let attempt = 0; attempt < 100 && !(await Bun.file(marker).exists()); attempt += 1) {
      await Bun.sleep(10);
    }
    expect(await Bun.file(marker).exists()).toBe(true);
    const blocked = run(root, ["--json", "restore", "--from", deleted.recovery_path]);
    expect(blocked.exitCode).toBe(1);
    expect(dec.decode(blocked.stdout)).toContain("state is busy");
    expect(await Bun.file(pkg).exists()).toBe(false);
  } finally {
    holder.kill();
    await holder.exited;
  }
});

test("restore preserves a differing destination", async () => {
  const root = await fixture(); const pkg = join(root, "library/demo");
  await mkdir(pkg, { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\noriginal\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const deleted = JSON.parse(dec.decode(run(root, ["--json", "delete", "demo", "--yes"]).stdout));
  await mkdir(pkg, { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\nnew unrelated content\n");
  const restored = run(root, ["--json", "restore", "--from", deleted.recovery_path]);
  expect(restored.exitCode).toBe(1);
  expect(dec.decode(restored.stdout)).toContain("different contents");
  expect(await readFile(join(pkg, "SKILL.md"), "utf8")).toContain("new unrelated content");
});

test("restore fails closed when a destination appears after preflight", async () => {
  const root = await fixture(); const pkg = join(root, "library/demo");
  await mkdir(pkg, { recursive: true }); await writeFile(join(pkg, "SKILL.md"), "name: demo\noriginal\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const deleted = JSON.parse(dec.decode(run(root, ["--json", "delete", "demo", "--yes"]).stdout));
  const restored = run(root, ["--json", "restore", "--from", deleted.recovery_path], { SKILLSYNC_TEST_RESTORE_COLLISION: "1" });
  expect(restored.exitCode).toBe(1);
  expect(await readFile(join(pkg, "SKILL.md"), "utf8")).toBe("external collision\n");
});
