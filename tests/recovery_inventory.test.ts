import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { childEnv, commandBinary } from "./test_harness";

const dec = new TextDecoder();
function env(root: string) {
  return { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library") };
}
function run(root: string, args: string[], extra: Record<string, string> = {}) {
  return Bun.spawnSync({ cmd: [commandBinary(extra), ...args], env: childEnv({ ...env(root), ...extra }), stdout: "pipe", stderr: "pipe" });
}

test("recovery inspect rejects an attacker-created package-shaped artifact", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const attacker = join(root, "config/recovery/attacker/package");
  await mkdir(attacker, { recursive: true });
  await writeFile(join(attacker, "SKILL.md"), "name: attacker\n");
  const listed = JSON.parse(dec.decode(run(root, ["--json", "recovery", "list"]).stdout));
  const artifact = listed.artifacts.find((item: any) => item.category === "unknown");
  expect(artifact).toBeDefined();
  const inspected = run(root, ["--json", "recovery", "inspect", artifact.id]);
  expect(inspected.exitCode).not.toBe(0);
});

test("recovery inspect rejects a fabricated marker copied from a real snapshot", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  expect(run(root, ["--json", "delete", "demo", "--yes"]).exitCode).toBe(0);
  const recoveryRoot = join(root, "config/recovery");
  const original = (await readdir(recoveryRoot))[0];
  const packageBytes = await readFile(join(recoveryRoot, original, "package/SKILL.md"));
  const markerBytes = await readFile(join(recoveryRoot, original, ".skillsync-deletion.json"));
  const forged = join(recoveryRoot, "forged-delete");
  await mkdir(join(forged, "package"), { recursive: true });
  await writeFile(join(forged, "package/SKILL.md"), packageBytes);
  await writeFile(join(forged, ".skillsync-deletion.json"), markerBytes);
  await rm(join(recoveryRoot, original), { recursive: true });
  const listed = JSON.parse(dec.decode(run(root, ["--json", "recovery", "list"]).stdout));
  expect(listed.count).toBe(1);
  const inspected = run(root, ["--json", "recovery", "inspect", listed.artifacts[0].id]);
  expect(inspected.exitCode).not.toBe(0);
});

test("recovery inspect validates a retained deletion snapshot", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  expect(run(root, ["--json", "delete", "demo", "--yes"]).exitCode).toBe(0);
  const listed = JSON.parse(dec.decode(run(root, ["--json", "recovery", "list"]).stdout));
  const inspected = run(root, ["--json", "recovery", "inspect", listed.artifacts[0].id]);
  expect(inspected.exitCode).toBe(0);
  expect(JSON.parse(dec.decode(inspected.stdout))).toMatchObject({ ok: true, id: listed.artifacts[0].id, category: "deletion", status: "retained", deletable: false });
});

test("recovery inspect rejects malformed and unknown ids without mutation", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  const before = await readdir(root);
  for (const id of ["bad", "recovery-0000000000000000"]) {
    const result = run(root, ["--json", "recovery", "inspect", id]);
    expect(result.exitCode).not.toBe(0);
    expect(dec.decode(result.stdout) + dec.decode(result.stderr)).not.toContain(id);
  }
  expect(await readdir(root)).toEqual(before);
  expect(await Bun.file(join(root, "config")).exists()).toBe(false);
});

test("recovery inspect is no-create when uninitialized", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  const result = run(root, ["--json", "recovery", "inspect", "recovery-0000000000000000"]);
  expect(result.exitCode).not.toBe(0);
  expect(await Bun.file(join(root, "config", "config.toml")).exists()).toBe(false);
  expect(await Bun.file(join(root, "config", "recovery")).exists()).toBe(false);
});

test("recovery inspect returns only bounded deletion evidence", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\nsecret-manifest-token\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  expect(run(root, ["--json", "delete", "demo", "--yes"]).exitCode).toBe(0);
  const listed = JSON.parse(dec.decode(run(root, ["--json", "recovery", "list"]).stdout));
  const result = run(root, ["--json", "recovery", "inspect", listed.artifacts[0].id]);
  const output = dec.decode(result.stdout) + dec.decode(result.stderr);
  expect(result.exitCode).toBe(0);
  expect(output).not.toContain("delete-demo");
  expect(output).not.toContain("secret-manifest-token");
  expect(output).not.toContain(join(root, "config"));
});

test("recovery inspect fails closed for a tampered snapshot and symlink entry", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-inspect-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  expect(run(root, ["--json", "delete", "demo", "--yes"]).exitCode).toBe(0);
  const recoveryRoot = join(root, "config/recovery");
  const artifact = (await readdir(recoveryRoot))[0];
  const listed = JSON.parse(dec.decode(run(root, ["--json", "recovery", "list"]).stdout));
  await writeFile(join(recoveryRoot, artifact, "package", "extra"), "tampered\n");
  const tampered = run(root, ["--json", "recovery", "inspect", listed.artifacts[0].id]);
  expect(tampered.exitCode).not.toBe(0);
  const outside = join(root, "outside");
  await writeFile(outside, "outside\n");
  await symlink(outside, join(recoveryRoot, artifact, "package", "linked"));
  const relisted = JSON.parse(dec.decode(run(root, ["--json", "recovery", "list"]).stdout));
  const linked = run(root, ["--json", "recovery", "inspect", relisted.artifacts[0].id]);
  expect(linked.exitCode).not.toBe(0);
  expect(await readFile(outside, "utf8")).toBe("outside\n");
});

test("recovery list reports an empty deterministic inventory without initialization", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  expect(JSON.parse(dec.decode(result.stdout))).toEqual({ ok: true, message: "ok", artifacts: [], count: 0 });
  expect(await Bun.file(join(root, "config/state.json")).exists()).toBe(false);
  expect(await Bun.file(join(root, "config/config.toml")).exists()).toBe(false);
  expect(await Bun.file(join(root, "config/state.lock")).exists()).toBe(false);
  expect(await Bun.file(join(root, "config/recovery")).exists()).toBe(false);
});

test("recovery list rejects a symlink package entry without following its target", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const artifact = join(root, "config/recovery", "delete-symlink-package");
  const target = join(root, "outside-package");
  await mkdir(target, { recursive: true });
  const targetFile = join(target, "SKILL.md");
  await writeFile(targetFile, "must remain unchanged\n");
  await mkdir(artifact, { recursive: true });
  await symlink(target, join(artifact, "package"));
  const before = await readFile(targetFile, "utf8");
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const body = JSON.parse(dec.decode(result.stdout));
  expect(body.artifacts[0]).toMatchObject({ category: "unknown", status: "invalid", deletable: false });
  expect(body.artifacts[0].reason).toBe("unrecognized_recovery_artifact");
  expect(await readFile(targetFile, "utf8")).toBe(before);

  const recovery = await readFile(join(import.meta.dir, "../src/recovery.rs"), "utf8");
  expect(recovery).not.toContain("path.join(\"package\").exists()");
});

test("recovery list rejects operational symlinks inside deletion snapshots without following them", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  expect(run(root, ["--json", "delete", "demo", "--yes"]).exitCode).toBe(0);
  const recovery = (await readdir(join(root, "config/recovery")))[0];
  const marker = join(root, "outside-marker");
  await writeFile(marker, "must remain unchanged\n");
  await symlink(marker, join(root, "config/recovery", recovery, "package/.env"));
  const before = await readFile(marker, "utf8");
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const body = JSON.parse(dec.decode(result.stdout));
  expect(body.artifacts[0]).toMatchObject({ category: "unknown", status: "invalid", deletable: false });
  expect(body.artifacts[0].reason).toBe("unrecognized_recovery_artifact");
  expect(await readFile(marker, "utf8")).toBe(before);
});

test("recovery list never emits attacker-controlled artifact names or metadata", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const sensitive = "delete-api-key-prod__credential-token__" + "a".repeat(80);
  await mkdir(join(root, "config/recovery", sensitive), { recursive: true });
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const text = dec.decode(result.stdout);
  expect(text).not.toContain(sensitive);
  const body = JSON.parse(text);
  expect(body.artifacts[0].id).toMatch(/^recovery-[a-f0-9]{16}$/);
});

test("recovery list does not trust a fabricated valid delete directory as owned", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const fabricated = join(root, "config/recovery", "delete-credential-backup");
  await mkdir(join(fabricated, "package"), { recursive: true });
  await writeFile(join(fabricated, "package/SKILL.md"), "name: demo\n");
  const before = await readdir(join(root, "config/recovery"));
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const body = JSON.parse(dec.decode(result.stdout));
  expect(body.artifacts[0]).toMatchObject({ category: "unknown", status: "invalid", deletable: false });
  expect(body.artifacts[0].reason).toBe("unrecognized_recovery_artifact");
  expect(await readdir(join(root, "config/recovery"))).toEqual(before);
});

test("recovery list is read-only and deterministic for a deletion snapshot", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const deleted = run(root, ["--json", "delete", "demo", "--yes"]);
  expect(deleted.exitCode).toBe(0);
  const before = await readdir(join(root, "config/recovery"));
  const first = run(root, ["--json", "recovery", "list"]);
  const second = run(root, ["--json", "recovery", "list"]);
  expect(first.exitCode).toBe(0);
  expect(second.exitCode).toBe(0);
  expect(dec.decode(first.stdout)).toBe(dec.decode(second.stdout));
  expect(JSON.parse(dec.decode(first.stdout)).count).toBe(1);
  expect(await readdir(join(root, "config/recovery"))).toEqual(before);
});

test("identical safe direct children receive distinct deterministic recovery ids", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const recoveryRoot = join(root, "config/recovery");
  const first = join(recoveryRoot, "snapshot-one", "package");
  const second = join(recoveryRoot, "snapshot-two", "package");
  await mkdir(first, { recursive: true });
  await mkdir(second, { recursive: true });
  await writeFile(join(first, "SKILL.md"), "name: demo\n");
  await writeFile(join(second, "SKILL.md"), "name: demo\n");
  expect(await readFile(join(first, "SKILL.md"))).toEqual(Buffer.from("name: demo\n"));
  const before = (await readdir(recoveryRoot)).sort();
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const body = JSON.parse(dec.decode(result.stdout));
  expect(body.count).toBe(2);
  expect(new Set(body.artifacts.map((item: { id: string }) => item.id)).size).toBe(2);
  expect((await readdir(recoveryRoot)).sort()).toEqual(before);
});

test("distinct unsafe direct children never receive colliding recovery ids", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const outside = join(root, "outside");
  await mkdir(join(root, "config/recovery"), { recursive: true });
  await writeFile(outside, "outside\n");
  await symlink(outside, join(root, "config/recovery", "unsafe-one"));
  await symlink(outside, join(root, "config/recovery", "unsafe-two"));
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const body = JSON.parse(dec.decode(result.stdout));
  expect(body.artifacts).toHaveLength(2);
  expect(new Set(body.artifacts.map((item: { id: string }) => item.id)).size).toBe(2);
});

test("recovery ids include operational-looking descendants without mutating snapshots", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const recoveryRoot = join(root, "config/recovery");
  const first = join(recoveryRoot, "snapshot-one", "package");
  const second = join(recoveryRoot, "snapshot-two", "package");
  await mkdir(first, { recursive: true });
  await mkdir(second, { recursive: true });
  await writeFile(join(first, "SKILL.md"), "name: demo\n");
  await writeFile(join(second, "SKILL.md"), "name: demo\n");
  await writeFile(join(first, ".env"), "TOKEN=one\n");
  await writeFile(join(second, ".env"), "TOKEN=two\n");
  const before = [await readFile(join(first, ".env"), "utf8"), await readFile(join(second, ".env"), "utf8")];
  const result = run(root, ["--json", "recovery", "list"]);
  expect(result.exitCode).toBe(0);
  const body = JSON.parse(dec.decode(result.stdout));
  expect(body.artifacts).toHaveLength(2);
  expect(new Set(body.artifacts.map((item: { id: string }) => item.id)).size).toBe(2);
  expect([await readFile(join(first, ".env"), "utf8"), await readFile(join(second, ".env"), "utf8")]).toEqual(before);
});

test("recovery inventory never uses path-following reads for descendants", async () => {
  const recovery = await readFile(join(import.meta.dir, "../src/recovery.rs"), "utf8");
  expect(recovery).not.toContain("fs::read(root.join(relative))");
  expect(recovery).toContain("open_entry_checked");
});

test("recovery dispatch verifies config identity before App::load", async () => {
  const main = await readFile(join(import.meta.dir, "../src/main.rs"), "utf8");
  const recovery = main.indexOf("let recovery_read = matches!(");
  const load = main.indexOf("let mut a = App::load(operation_lock)?;");
  const segment = main.slice(recovery, load);
  expect(recovery).toBeGreaterThanOrEqual(0);
  expect(segment).toContain("if relationship_verify || recovery_read");
  expect(segment).toContain("verify_config_identity");
});

test("recovery list fails closed when the retained root is replaced", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-recovery-list-"));
  await mkdir(join(root, "library/demo"), { recursive: true });
  await writeFile(join(root, "library/demo/SKILL.md"), "name: demo\n");
  expect(run(root, ["--json", "init"]).exitCode).toBe(0);
  const recovery = join(root, "config/recovery");
  await mkdir(join(recovery, "snapshot"), { recursive: true });
  const result = run(root, ["--json", "recovery", "list"], { SKILLSYNC_TEST_REPLACE_RECOVERY_ROOT: "1" });
  expect(result.exitCode).not.toBe(0);
  expect(dec.decode(result.stdout) + dec.decode(result.stderr)).toContain("recovery root changed during inventory");
  expect(await Bun.file(join(recovery, "snapshot")).exists()).toBe(false);
  expect((await readdir(join(root, "config/recovery.replaced"))).sort()).toEqual(["snapshot"]);
});
