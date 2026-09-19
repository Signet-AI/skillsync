import { expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { commandBinary, childEnv } from "./test_harness";

function relationshipKey(source: string, path: string): string {
  return `rel-${createHash("sha256").update(source).update(new Uint8Array([0])).update(path).digest("hex")}`;
}

test("relationship verification requires an initialized target", () => {
  const root = mkdtempSync(join(tmpdir(), "skillsync-relationship-red-"));
  const result = Bun.spawnSync({
    cmd: [commandBinary(), "--json", "state", "relationship", "verify", "--relationship", "rel-test"],
    env: childEnv({ SKILLSYNC_CONFIG_DIR: join(root, "config") }),
    stdout: "pipe", stderr: "pipe",
  });
  expect(result.exitCode).not.toBe(0);
  const output = new TextDecoder().decode(result.stdout) + new TextDecoder().decode(result.stderr);
  expect(output).toContain("target is not initialized");
});

test("relationship verification never emits unsafe persisted sources", async () => {
  const root = mkdtempSync(join(tmpdir(), "skillsync-relationship-source-red-"));
  const config = join(root, "config");
  const library = join(root, "library");
  const env = childEnv({ SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library });
  const init = Bun.spawnSync({ cmd: [commandBinary(), "--json", "init", "--library", library], env, stdout: "pipe", stderr: "pipe" });
  expect(init.exitCode).toBe(0);
  const statePath = join(config, "state.json");
  const state = JSON.parse(await Bun.file(statePath).text());
  const relationship = relationshipKey("https://user:secret@example.test/org/repo", ".");
  state.subscriptions[relationship] = {
    skill: "demo", source: "https://user:secret@example.test/org/repo", branch: "main", source_path: ".",
    baseline_path: join(config, "baselines/https-user-secret-example.test-org-repo"), baseline_hash: "0".repeat(64),
    baseline_source: "https://user:secret@example.test/org/repo", baseline_source_path: ".", local_path: join(library, "demo"),
    status: "synced", recovery_path: null, conflict_selection: null, last_sync: 0, update_count: 0,
    branch_policy: { kind: "remote_default" }, resolved_commit: null, resolved_tree: null,
  };
  await Bun.write(statePath, JSON.stringify(state));
  const result = Bun.spawnSync({ cmd: [commandBinary(), "--json", "state", "relationship", "verify", "--relationship", relationship], env, stdout: "pipe", stderr: "pipe" });
  const output = new TextDecoder().decode(result.stdout) + new TextDecoder().decode(result.stderr);
  expect(result.exitCode).toBe(0);
  expect(output).not.toContain("secret");
  expect(output).not.toContain("user:");
  const before = await Bun.file(statePath).text();
  const second = Bun.spawnSync({ cmd: [commandBinary(), "--json", "state", "relationship", "verify", "--relationship", relationship], env, stdout: "pipe", stderr: "pipe" });
  expect(new TextDecoder().decode(second.stdout)).toBe(new TextDecoder().decode(result.stdout));
  expect(await Bun.file(statePath).text()).toBe(before);
});

test("relationship verification rejects conflict status without valid recovery evidence", async () => {
  const root = mkdtempSync(join(tmpdir(), "skillsync-relationship-conflict-red-"));
  const config = join(root, "config");
  const library = join(root, "library");
  const env = childEnv({ SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library });
  const init = Bun.spawnSync({ cmd: [commandBinary(), "--json", "init", "--library", library], env, stdout: "pipe", stderr: "pipe" });
  expect(init.exitCode).toBe(0);
  const statePath = join(config, "state.json");
  const state = JSON.parse(await Bun.file(statePath).text());
  const relationship = relationshipKey("https://example.test/org/repo", ".");
  state.subscriptions[relationship] = {
    skill: "demo", source: "https://example.test/org/repo", branch: "main", source_path: ".",
    baseline_path: join(config, "baselines/rel"), baseline_hash: "0".repeat(64),
    baseline_source: "https://example.test/org/repo", baseline_source_path: ".", local_path: join(library, "demo"),
    status: "conflict", recovery_path: join(config, "recovery/missing"), conflict_selection: "local", last_sync: 0, update_count: 0,
    branch_policy: { kind: "remote_default" }, resolved_commit: null, resolved_tree: null,
  };
  await Bun.write(statePath, JSON.stringify(state));
  const result = Bun.spawnSync({ cmd: [commandBinary(), "--json", "state", "relationship", "verify", "--relationship", relationship], env, stdout: "pipe", stderr: "pipe" });
  expect(result.exitCode).toBe(0);
  const report = JSON.parse(new TextDecoder().decode(result.stdout));
  expect(report.status).toBe("blocked");
  expect(report.blockers).toContain("conflict_recovery_evidence");
  expect(report.checks.find((check: any) => check.check === "state_stage_preflight")).toMatchObject({ status: "blocked" });
  expect(report.blockers).toContain("activation_unsupported");
});

test("relationship verification derives state-stage blockers from current package state", async () => {
  const root = mkdtempSync(join(tmpdir(), "skillsync-relationship-preflight-red-"));
  const config = join(root, "config");
  const library = join(root, "library");
  const env = childEnv({ SKILLSYNC_CONFIG_DIR: config, SKILLSYNC_LIBRARY: library });
  expect(Bun.spawnSync({ cmd: [commandBinary(), "--json", "init", "--library", library], env, stdout: "pipe", stderr: "pipe" }).exitCode).toBe(0);
  const statePath = join(config, "state.json");
  const state = JSON.parse(await Bun.file(statePath).text());
  const relationship = relationshipKey("https://example.test/org/preflight", ".");
  state.subscriptions[relationship] = {
    skill: "missing", source: "https://example.test/org/preflight", branch: "main", source_path: ".",
    baseline_path: join(config, "baselines/missing"), baseline_hash: "0".repeat(64),
    baseline_source: "https://example.test/org/preflight", baseline_source_path: ".", local_path: join(library, "missing"),
    status: "synced", recovery_path: null, conflict_selection: null, last_sync: 0, update_count: 0,
    branch_policy: { kind: "remote_default" }, resolved_commit: null, resolved_tree: null,
  };
  await Bun.write(statePath, JSON.stringify(state));
  const result = Bun.spawnSync({ cmd: [commandBinary(), "--json", "state", "relationship", "verify", "--relationship", relationship], env, stdout: "pipe", stderr: "pipe" });
  expect(result.exitCode).toBe(0);
  const report = JSON.parse(new TextDecoder().decode(result.stdout));
  expect(report.checks.find((check: any) => check.check === "state_stage_preflight")).toMatchObject({ status: "blocked" });
  expect(report.blockers).toEqual(["activation_unsupported", "baseline_integrity", "baseline_recovery_evidence_missing", "package_contents_missing", "remote_credentials_required", "subscription_invariants"]);
});
