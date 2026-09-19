import { expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { commandBinary } from "./test_harness";

const binary = commandBinary();
const dec = new TextDecoder();

async function packageFixture(prefix: string) {
  const root = await mkdtemp(join(tmpdir(), prefix));
  const source = join(root, "source");
  await mkdir(join(source, "package"), { recursive: true });
  const payload = Buffer.from("name: demo\n\nstable package bytes\n");
  await writeFile(join(source, "package", "SKILL.md"), payload);
  const hash = createHash("sha256").update(payload).digest("hex");
  const entries = [{ path: "SKILL.md", sha256: hash, mode: 0o644 }];
  const tree_hash = createHash("sha256").update(`SKILL.md\0${hash}\0${0o644}\n`).digest("hex");
  await writeFile(join(source, "manifest.json"), JSON.stringify({ format: "skillsync-package-transfer", version: 1, package: "demo", tree_hash, entries }));
  return { root, source, payload };
}

async function stage(source: string, out: string, env: Record<string, string>) {
  return Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "stage", "--from", source, "--out", out], env: { ...process.env, ...env }, stdout: "pipe", stderr: "pipe" });
}

test("package staging rejects every configured owned root before creating missing parents", async () => {
  const f = await packageFixture("skillsync-package-owned-root-red-");
  try {
    const config = join(f.root, "config");
    const library = join(f.root, "library");
    await mkdir(config, { recursive: true });
    await writeFile(join(config, "config.toml"), `library = ${JSON.stringify(library)}\\n`);
    const env = { SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library };
    const outputs = [config, join(config, "missing", "artifact"), join(config, "state.json", "child"), join(config, "baselines", "new", "artifact"), join(config, "recovery", "new", "artifact"), library, join(library, "missing", "artifact")];
    for (const out of outputs) {
      const result = await stage(f.source, out, env);
      expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
      expect(await readFile(join(f.source, "package", "SKILL.md"))).toEqual(f.payload);
      expect(await Bun.file(join(out, "manifest.json")).exists()).toBe(false);
      if (out !== config) expect(await Bun.file(out).exists(), out).toBe(false);
    }
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("package staging accepts a valid external output with no config initialization", async () => {
  const f = await packageFixture("skillsync-package-external-red-");
  try {
    const config = join(f.root, "config");
    const external = join(f.root, "external", "artifact");
    const result = await stage(f.source, external, { SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: join(f.root, "library") });
    expect(result.exitCode, dec.decode(result.stderr)).toBe(0);
    expect(await Bun.file(join(external, "manifest.json")).exists()).toBe(true);
    expect(await Bun.file(join(config, "state.json")).exists()).toBe(false);
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("package transfer stage rejects an output nested inside its source", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-package-overlap-red-"));
  try {
    const source = join(root, "source");
    const packageDir = join(source, "package");
    const payload = Buffer.from("stable source bytes");
    await mkdir(packageDir, { recursive: true });
    await writeFile(join(packageDir, "SKILL.md"), payload);
    const hash = createHash("sha256").update(payload).digest("hex");
    const treeHash = createHash("sha256").update(`SKILL.md\\0${hash}\\0420\\n`).digest("hex");
    await writeFile(join(source, "manifest.json"), JSON.stringify({ format: "skillsync-package-transfer", version: 1, package: "demo", tree_hash: treeHash, entries: [{ path: "SKILL.md", sha256: hash, mode: 0o644 }] }));
    const destination = join(source, "nested", "artifact");
    const before = await readFile(join(packageDir, "SKILL.md"));
    const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "stage", "--from", source, "--out", destination], stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
    expect(await readFile(join(packageDir, "SKILL.md"))).toEqual(before);
    await expect(readFile(destination)).rejects.toThrow();
    expect(await Bun.file(join(source, "nested")).exists()).toBe(false);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("package transfer stage rejects equal and aliased source outputs before creation", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-package-overlap-alias-"));
  try {
    const source = join(root, "source");
    await mkdir(join(source, "package"), { recursive: true });
    for (const out of [source, join(root, "source", "..", "source")]) {
      const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "stage", "--from", source, "--out", out], stdout: "pipe", stderr: "pipe" });
      expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
      expect(await Bun.file(join(source, "package", "SKILL.md")).exists()).toBe(false);
    }
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("package transfer stage rejects an ancestor output before creation", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-package-overlap-ancestor-"));
  try {
    const source = join(root, "source", "nested");
    await mkdir(join(source, "package"), { recursive: true });
    const out = join(root, "source");
    const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "stage", "--from", source, "--out", out], stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
    expect(await Bun.file(out).exists()).toBe(false);
    expect(await Bun.file(join(out, "manifest.json")).exists()).toBe(false);
  } finally { await rm(root, { recursive: true, force: true }); }
});
test("package transfer stage publishes the strict external artifact layout", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-package-external-"));
  try {
    const source = join(root, "source");
    await mkdir(join(source, "package", "nested"), { recursive: true });
    const payload = Buffer.from("name: demo\n\nexternal output bytes\n");
    const nested = Buffer.from("nested resource bytes");
    await writeFile(join(source, "package", "SKILL.md"), payload);
    await writeFile(join(source, "package", "nested", "resource.txt"), nested);
    const entries = [
      { path: "SKILL.md", sha256: createHash("sha256").update(payload).digest("hex"), mode: 0o644 },
      { path: "nested/resource.txt", sha256: createHash("sha256").update(nested).digest("hex"), mode: 0o644 },
    ];
    const treeHash = createHash("sha256").update(entries.map((e) => `${e.path}\0${e.sha256}\0${e.mode}\n`).join("")).digest("hex");
    await writeFile(join(source, "manifest.json"), JSON.stringify({ format: "skillsync-package-transfer", version: 1, package: "demo", tree_hash: treeHash, entries }));
    const beforeSkill = await readFile(join(source, "package", "SKILL.md"));
    const beforeNested = await readFile(join(source, "package", "nested", "resource.txt"));
    const destination = join(root, "output", "artifact");
    const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "stage", "--from", source, "--out", destination], stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode, dec.decode(result.stderr)).toBe(0);
    expect(JSON.parse(await Bun.file(join(destination, "manifest.json")).text())).toEqual(JSON.parse(await Bun.file(join(source, "manifest.json")).text()));
    expect(await readFile(join(destination, "package", "SKILL.md"))).toEqual(payload);
    expect(await readFile(join(destination, "package", "nested", "resource.txt"))).toEqual(nested);
    expect(await readFile(join(source, "package", "SKILL.md"))).toEqual(beforeSkill);
    expect(await readFile(join(source, "package", "nested", "resource.txt"))).toEqual(beforeNested);
  } finally { await rm(root, { recursive: true, force: true }); }
});

test("package transfer inspect rejects a source alias through a symlinked ancestor", async () => {
  const f = await packageFixture("skillsync-package-inspect-alias-red-");
  try {
    const aliasParent = join(f.root, "alias-parent");
    await symlink(f.root, aliasParent, "dir");
    const aliasedSource = join(aliasParent, "source");
    const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "inspect", "--from", aliasedSource], stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
  } finally { await rm(f.root, { recursive: true, force: true }); }
});


test("package install verification failure leaves no canonical package or parent side effect", async () => {
  const f = await packageFixture("skillsync-package-install-red-");
  try {
    const config = join(f.root, "config");
    const library = join(f.root, "library");
    const stagePath = join(f.root, "stage");
    const env = { ...process.env, SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library, SKILLSYNC_TEST_FAIL_PACKAGE_INSTALL_VERIFY: "1" };
    const hookBinary = commandBinary({ SKILLSYNC_TEST_FAIL_PACKAGE_INSTALL_VERIFY: "1" });
    const initialized = Bun.spawnSync({ cmd: [hookBinary, "--json", "init", "--library", library], env, stdout: "pipe", stderr: "pipe" });
    expect(initialized.exitCode, dec.decode(initialized.stderr)).toBe(0);
    const stateBefore = await readFile(join(config, "state.json"));
    const staged = await stage(f.source, stagePath, env);
    expect(staged.exitCode, dec.decode(staged.stderr)).toBe(0);
    const result = Bun.spawnSync({ cmd: [hookBinary, "--json", "state", "package", "install", "--from", stagePath, "--yes"], env, stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode).not.toBe(0);
    expect(await Bun.file(join(library, "demo")).exists()).toBe(false);
    expect(await readFile(join(config, "state.json"))).toEqual(stateBefore);
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("package install valid artifact and replay are deterministic", async () => {
  const f = await packageFixture("skillsync-package-install-green-");
  try {
    const config = join(f.root, "config");
    const library = join(f.root, "library");
    const stagePath = join(f.root, "stage");
    const env = { ...process.env, SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library };
    const initialized = Bun.spawnSync({ cmd: [binary, "--json", "init", "--library", library], env, stdout: "pipe", stderr: "pipe" });
    expect(initialized.exitCode, dec.decode(initialized.stderr)).toBe(0);
    expect((await stage(f.source, stagePath, env)).exitCode).toBe(0);
    const first = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "install", "--from", stagePath, "--yes"], env, stdout: "pipe", stderr: "pipe" });
    expect(first.exitCode, dec.decode(first.stderr)).toBe(0);
    const second = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "install", "--from", stagePath, "--yes"], env, stdout: "pipe", stderr: "pipe" });
    expect(second.exitCode, dec.decode(second.stderr)).toBe(0);
    expect(JSON.parse(dec.decode(second.stdout)).status).toBe("already_present");
    expect(await Bun.file(join(library, "demo", "SKILL.md")).exists()).toBe(true);
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("package install rejects a staged artifact whose SKILL.md name mismatches the transfer package", async () => {
  const f = await packageFixture("skillsync-package-identity-red-");
  try {
    const config = join(f.root, "config");
    const library = join(f.root, "library");
    const stagePath = join(f.root, "stage");
    const env = { ...process.env, SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library };
    const initialized = Bun.spawnSync({ cmd: [binary, "--json", "init", "--library", library], env, stdout: "pipe", stderr: "pipe" });
    expect(initialized.exitCode, dec.decode(initialized.stderr)).toBe(0);
    expect((await stage(f.source, stagePath, env)).exitCode).toBe(0);
    const mismatched = Buffer.from("name: other\n\n# valid multiline content\n");
    await writeFile(join(stagePath, "package", "SKILL.md"), mismatched);
    const hash = createHash("sha256").update(mismatched).digest("hex");
    const tree_hash = createHash("sha256").update(`SKILL.md\0${hash}\0${0o644}\n`).digest("hex");
    await writeFile(join(stagePath, "manifest.json"), JSON.stringify({ format: "skillsync-package-transfer", version: 1, package: "demo", tree_hash, entries: [{ path: "SKILL.md", sha256: hash, mode: 0o644 }] }));
    const stateBefore = await readFile(join(config, "state.json"));
    const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "install", "--from", stagePath, "--yes"], env, stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
    expect(await Bun.file(join(library, "demo")).exists()).toBe(false);
    expect(await readFile(join(config, "state.json"))).toEqual(stateBefore);
    expect(await Bun.file(join(library, "demo", "SKILL.md")).exists()).toBe(false);
  } finally { await rm(f.root, { recursive: true, force: true }); }
});

test("package transfer inspect rejects a package without SKILL.md identity", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-package-red-"));
  try {
    const source = join(root, "source");
    await mkdir(join(source, "package"), { recursive: true });
    await writeFile(join(source, "manifest.json"), JSON.stringify({ format: "skillsync-package-transfer", version: 1, package: "demo", tree_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", entries: [] }));
    const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "package", "inspect", "--from", source], stdout: "pipe", stderr: "pipe" });
    expect(result.exitCode, dec.decode(result.stderr)).not.toBe(0);
  } finally { await rm(root, { recursive: true, force: true }); }
});
