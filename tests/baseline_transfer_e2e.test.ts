import { describe, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createHash } from "node:crypto";
import { commandBinary } from "./test_harness";

const dec = new TextDecoder();
const binary = commandBinary();

async function fixture(sourceIdentity = "https://example.com/repo.git") {
  const root = await mkdtemp(join(tmpdir(), "skillsync-baseline-red-"));
  const config = join(root, "config");
  const library = join(root, "library");
  const source = join(root, "source");
  const relationship = "rel-" + createHash("sha256").update(sourceIdentity + "\0pkg").digest("hex");
  const baseline = join(config, "baselines", relationship);
  await mkdir(join(baseline), { recursive: true });
  await mkdir(source, { recursive: true });
  await writeFile(join(baseline, "SKILL.md"), "name: demo\n\n# baseline\n");
  const bytes = await readFile(join(baseline, "SKILL.md"));
  const hash = "aef70f39460b986087cd8e304f4e864573854fa21a7a31a17120c835b5bd4337";
  await mkdir(config, { recursive: true });
  const init = Bun.spawnSync({ cmd: [binary, "--json", "init", "--library", library], env: { ...process.env, SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library }, stdout: "pipe", stderr: "pipe" });
  if (init.exitCode !== 0) throw new Error(dec.decode(init.stderr));
  const statePath = join(config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions[relationship] = { skill: "demo", source: sourceIdentity, branch: "main", branch_policy: { kind: "remote_default" }, source_path: "pkg", baseline_path: baseline, baseline_hash: hash, baseline_source: sourceIdentity, baseline_source_path: ".", local_path: join(library, "demo"), status: "synced", recovery_path: null, last_sync: 0, update_count: 0, resolved_commit: "a".repeat(40), resolved_tree: "b".repeat(40) };
  await writeFile(statePath, JSON.stringify(state));
  return { root, config, library, baseline, source, relationship, env: { ...process.env, SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library } };
}

describe("baseline transfer command surface", () => {
  test("export rejects absolute local source without creating output or changing state", async () => {
    const f = await fixture("/home/user/repo");
    try {
      const statePath = join(f.config, "state.json");
      const beforeState = await readFile(statePath);
      const beforeBaseline = await readFile(join(f.baseline, "SKILL.md"));
      const out = join(f.root, "new-parent", "artifact");
      const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "baseline", "export", "--relationship", f.relationship, "--out", out], env: f.env, stdout: "pipe", stderr: "pipe" });
      expect(result.exitCode, dec.decode(result.stderr) + dec.decode(result.stdout)).not.toBe(0);
      expect(await Bun.file(out).exists()).toBe(false);
      expect(await Bun.file(join(f.root, "new-parent")).exists()).toBe(false);
      expect(await readFile(statePath)).toEqual(beforeState);
      expect(await readFile(join(f.baseline, "SKILL.md"))).toEqual(beforeBaseline);
      expect(await Bun.file(join(f.library, "demo", "SKILL.md")).exists()).toBe(false);
    } finally { await rm(f.root, { recursive: true, force: true }); }
  });
  test("export rejects missing descendants of every Skillsync-owned root and source overlap before creation", async () => {
    const f = await fixture();
    try {
      const outputs = [join(f.library, "missing"), join(f.config, "missing"), join(f.config, "baselines", "new"), join(f.config, "recovery", "new"), join(f.source, "pkg", "nested")];
      for (const out of outputs) {
        const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "baseline", "export", "--relationship", f.relationship, "--out", out], env: f.env, stdout: "pipe", stderr: "pipe" });
        expect(result.exitCode, dec.decode(result.stderr) + dec.decode(result.stdout)).not.toBe(0);
        expect(await Bun.file(join(out, "manifest.json")).exists(), out).toBe(false);
      }
    } finally { await rm(f.root, { recursive: true, force: true }); }
  });

  test("inspect includes validated portable provenance fields", async () => {
    const f = await fixture();
    try {
      const artifact = join(f.root, "external");
      const exported = Bun.spawnSync({ cmd: [binary, "--json", "state", "baseline", "export", "--relationship", f.relationship, "--out", artifact], env: f.env, stdout: "pipe", stderr: "pipe" });
      expect(exported.exitCode, dec.decode(exported.stderr)).toBe(0);
      const result = Bun.spawnSync({ cmd: [binary, "--json", "state", "baseline", "inspect", "--from", artifact], env: f.env, stdout: "pipe", stderr: "pipe" });
      expect(result.exitCode).toBe(0);
      const output = JSON.parse(dec.decode(result.stdout));
      expect(output.branch_policy).toEqual({ kind: "remote_default" });
      expect(output.resolved_commit).toBe("a".repeat(40));
      expect(output.resolved_tree).toBe("b".repeat(40));
      const forged = JSON.parse(await readFile(join(artifact, "manifest.json"), "utf8"));
      forged.source = "/home/user/repo";
      forged.relationship = "rel-" + createHash("sha256").update("/home/user/repo\0pkg").digest("hex");
      await writeFile(join(artifact, "manifest.json"), JSON.stringify(forged));
      const rejected = Bun.spawnSync({ cmd: [binary, "--json", "state", "baseline", "inspect", "--from", artifact], env: f.env, stdout: "pipe", stderr: "pipe" });
      expect(rejected.exitCode, dec.decode(rejected.stderr) + dec.decode(rejected.stdout)).not.toBe(0);
    } finally { await rm(f.root, { recursive: true, force: true }); }
  });
});
