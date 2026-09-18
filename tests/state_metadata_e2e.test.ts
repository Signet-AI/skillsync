import { expect, test, afterEach } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";

const binary = process.env.SKILLSYNC_BIN ?? join(import.meta.dir, "../target/debug/skillsync");
const dec = new TextDecoder();
const roots: string[] = [];
function run(root: string, args: string[], ok = true) {
  const r = Bun.spawnSync({ cmd: [binary, "--json", ...args], env: { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library") }, stdout: "pipe", stderr: "pipe" });
  expect(r.exitCode, dec.decode(r.stderr)).toBe(ok ? 0 : 1);
  return JSON.parse(dec.decode(r.stdout));
}
async function fixture() { const root = await mkdtemp(join(tmpdir(), "skillsync-state-meta-")); roots.push(root); await mkdir(join(root, "library", "demo"), { recursive: true }); await writeFile(join(root, "library", "demo", "SKILL.md"), "name: demo\n"); run(root, ["init"]); run(root, ["set", "create", "core"]); run(root, ["set", "add", "core", "demo"]); return root; }
afterEach(async () => { await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true }))); });

test("state apply-sets rejects duplicate members before persisting", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const statePath = join(root, "config", "state.json");
  run(root, ["state", "export", "--out", bundle]);
  const value = JSON.parse(await readFile(bundle, "utf8"));
  value.sets = { incoming: { members: ["library:demo", "library:demo"] } };
  await writeFile(bundle, JSON.stringify(value));
  const before = await readFile(statePath, "utf8");
  const result = run(root, ["state", "apply-sets", "--from", bundle, "--yes"], false);
  expect(result.ok).toBe(false);
  expect(await readFile(statePath, "utf8")).toBe(before);
});

test("state apply-sets applies a validated set and replays idempotently", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json");
  run(root, ["state", "export", "--out", bundle]);
  const value = JSON.parse(await readFile(bundle, "utf8"));
  value.sets.incoming = { members: ["library:demo"] }; delete value.sets.core;
  await writeFile(bundle, JSON.stringify(value));
  expect(run(root, ["state", "apply-sets", "--from", bundle, "--yes"]).sets).toEqual([{ set: "incoming", status: "added" }]);
  expect(run(root, ["state", "apply-sets", "--from", bundle, "--yes"]).sets).toEqual([{ set: "incoming", status: "already_present" }]);
});

test("state apply-sets rejects non-set records and divergent sets without mutation", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const statePath = join(root, "config", "state.json");
  run(root, ["state", "export", "--out", bundle]); const before = await readFile(statePath, "utf8");
  const value = JSON.parse(await readFile(bundle, "utf8")); value.sets.core = { members: ["library:other"] }; value.subscriptions = { forbidden: {} };
  await writeFile(bundle, JSON.stringify(value));
  expect(run(root, ["state", "apply-sets", "--from", bundle, "--yes"], false).ok).toBe(false);
  expect(await readFile(statePath, "utf8")).toBe(before);
  delete value.subscriptions; await writeFile(bundle, JSON.stringify(value));
  expect(run(root, ["state", "apply-sets", "--from", bundle, "--yes"], false).ok).toBe(false);
  expect(await readFile(statePath, "utf8")).toBe(before);
});

test("state stage writes a deterministic non-activating evidence plan", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  run(root, ["state", "export", "--out", bundle]);
  const result = run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  expect(result.format).toBe("skillsync-state-stage-plan");
  const value = JSON.parse(await readFile(plan, "utf8"));
  expect(value.version).toBe(1); expect(value.bundle_hash).toMatch(/^[0-9a-f]{64}$/);
  expect(value.non_activating).toBe(true); expect(value.target.library).toBe(join(root, "library"));
  expect(value.records.every((record: any) => record.preflight?.readiness && Array.isArray(record.preflight.blockers))).toBe(true);
});

test("absent set and local adoption records are blocked by unsupported activation", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  await mkdir(join(root, "library", "other"));
  await writeFile(bundle, JSON.stringify({
    format: "skillsync-state-metadata", version: 1, metadata_only: true,
    subscriptions: {}, publications: {}, pending_publications: {},
    sets: { incoming: { members: ["library:demo"] } },
    local_adoptions: { "local:other": { skill: "other", source_package: ".", content_hash: "a".repeat(64), status: "adopted" } },
  }));
  run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  const value = JSON.parse(await readFile(plan, "utf8"));
  for (const record of value.records) {
    expect(record.preflight.target_available).toBe(true);
    expect(record.preflight.blockers).toContain("activation_unsupported");
    expect(record.preflight.readiness).toBe("blocked");
  }
  const forged = structuredClone(value);
  forged.records[0].preflight.readiness = "ready";
  await writeFile(plan, JSON.stringify(forged));
  expect(run(root, ["state", "inspect-plan", "--plan", plan], false).ok).toBe(false);
});

test("state inspect-plan rejects a forged subscription blocker vector before mutation", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  const source = "https://example.com/skills.git"; const sourcePath = ".";
  const key = "rel-" + createHash("sha256").update(Buffer.from(source + "\0" + sourcePath)).digest("hex");
  const subscription = { skill: "demo", source, branch: "main", source_path: sourcePath, baseline_hash: "c".repeat(64), baseline_source: source, baseline_source_path: sourcePath, status: "synced", conflict_selection: null, last_sync: 1, update_count: 0, resolved_commit: "a".repeat(40), resolved_tree: "b".repeat(40) };
  await writeFile(bundle, JSON.stringify({ format: "skillsync-state-metadata", version: 1, metadata_only: true, subscriptions: { [key]: subscription }, publications: {}, pending_publications: {}, sets: {}, local_adoptions: {} }));
  run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  const before = await readFile(join(root, "config", "state.json"), "utf8");
  const forged = JSON.parse(await readFile(plan, "utf8"));
  forged.records[0].preflight.blockers = forged.records[0].preflight.blockers.filter((x: string) => x !== "remote_credentials_required");
  await writeFile(plan, JSON.stringify(forged));
  expect(run(root, ["state", "inspect-plan", "--plan", plan], false).ok).toBe(false);
  expect(await readFile(join(root, "config", "state.json"), "utf8")).toBe(before);
  expect(await Bun.file(join(root, "library", "demo", "SKILL.md")).exists()).toBe(true);
});

test("state stage orders generated conflict blockers for an unavailable subscription target", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  const source = "https://example.com/skills.git"; const sourcePath = ".";
  const key = "rel-" + createHash("sha256").update(Buffer.from(source + "\0" + sourcePath)).digest("hex");
  const subscription = { skill: "demo", source, branch: "main", source_path: sourcePath, baseline_path: join(root, "library", "demo"), baseline_hash: "c".repeat(64), baseline_source: source, baseline_source_path: sourcePath, local_path: join(root, "library", "demo"), status: "synced", recovery_path: null, conflict_selection: null, last_sync: 1, update_count: 0, resolved_commit: "a".repeat(40), resolved_tree: "b".repeat(40) };
  const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions = { [key]: subscription }; await writeFile(statePath, JSON.stringify(state));
  await rm(join(root, "library", "demo"), { recursive: true, force: true });
  const incoming = { skill: "demo", source, branch: "main", source_path: sourcePath, baseline_hash: "d".repeat(64), baseline_source: source, baseline_source_path: sourcePath, status: "synced", conflict_selection: null, last_sync: 1, update_count: 0, resolved_commit: "a".repeat(40), resolved_tree: "b".repeat(40) };
  await writeFile(bundle, JSON.stringify({ format: "skillsync-state-metadata", version: 1, metadata_only: true, subscriptions: { [key]: incoming }, publications: {}, pending_publications: {}, sets: {}, local_adoptions: {} }));
  run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  const value = JSON.parse(await readFile(plan, "utf8"));
  expect(value.records[0].classification).toBe("conflict");
  expect(value.records[0].preflight.target_available).toBe(false);
  expect(run(root, ["state", "inspect-plan", "--plan", plan]).ok).toBe(true);
});

test("state inspect-plan validates a staged plan and reports target availability", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  run(root, ["state", "export", "--out", bundle]);
  run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  const before = await readFile(join(root, "config", "state.json"), "utf8");
  const result = run(root, ["state", "inspect-plan", "--plan", plan, "--from", bundle]);
  expect(result.format).toBe("skillsync-state-stage-plan"); expect(result.status).toBe("fresh");
  expect(result.record_count).toBeGreaterThan(0); expect(await readFile(join(root, "config", "state.json"), "utf8")).toBe(before);
});

test("state stage preserves equivalent provenance-bearing subscriptions as already present and fresh", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  const source = "https://example.com/skills.git"; const sourcePath = ".";
  const key = "rel-" + createHash("sha256").update(Buffer.from(source + "\0" + sourcePath)).digest("hex");
  const provenance = { resolved_commit: "a".repeat(40), resolved_tree: "b".repeat(40) };
  const subscription = { skill: "demo", source, branch: "main", source_path: sourcePath, baseline_path: join(root, "library", "demo"), local_path: join(root, "library", "demo"), baseline_hash: "c".repeat(64), baseline_source: source, baseline_source_path: sourcePath, status: "synced", conflict_selection: null, last_sync: 1, update_count: 0, ...provenance };
  const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions = { [key]: subscription }; await writeFile(statePath, JSON.stringify(state));
  const exported = { format: "skillsync-state-metadata", version: 1, metadata_only: true, subscriptions: { [key]: { skill: subscription.skill, source: subscription.source, branch: subscription.branch, source_path: subscription.source_path, baseline_hash: subscription.baseline_hash, baseline_source: subscription.baseline_source, baseline_source_path: subscription.baseline_source_path, status: subscription.status, conflict_selection: subscription.conflict_selection, last_sync: subscription.last_sync, update_count: subscription.update_count, ...provenance } }, publications: {}, pending_publications: {}, sets: {}, local_adoptions: {} };
  await writeFile(bundle, JSON.stringify(exported));
  const staged = run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  expect(JSON.parse(await readFile(plan, "utf8")).records[0].classification).toBe("already_present");
  expect(run(root, ["state", "inspect-plan", "--plan", plan, "--from", bundle]).status).toBe("fresh");
  expect(staged.record_count).toBe(1);
});

test("state stage rejects mismatched explicit branch policy before mutation", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  const source = "https://example.com/skills.git"; const sourcePath = ".";
  const key = "rel-" + createHash("sha256").update(Buffer.from(source + "\\0" + sourcePath)).digest("hex");
  const subscription = { skill: "demo", source, branch: "main", branch_policy: { kind: "explicit", name: "release" }, source_path: sourcePath, baseline_hash: "c".repeat(64), baseline_source: source, baseline_source_path: sourcePath, status: "synced", conflict_selection: null, last_sync: 1, update_count: 0, resolved_commit: "a".repeat(40), resolved_tree: "b".repeat(40) };
  const statePath = join(root, "config", "state.json"); const before = await readFile(statePath, "utf8");
  await writeFile(bundle, JSON.stringify({ format: "skillsync-state-metadata", version: 1, metadata_only: true, subscriptions: { [key]: subscription }, publications: {}, pending_publications: {}, sets: {}, local_adoptions: {} }));
  expect(run(root, ["state", "stage", "--from", bundle, "--plan", plan], false).ok).toBe(false);
  expect(await readFile(statePath, "utf8")).toBe(before);
  expect(await Bun.file(plan).exists()).toBe(false);
});

test("state inspect-plan rejects forged derived paths and duplicate records", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  run(root, ["state", "export", "--out", bundle]); run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  const value = JSON.parse(await readFile(plan, "utf8")); value.records[0].derived_local_path = "/forged"; await writeFile(plan, JSON.stringify(value));
  expect(run(root, ["state", "inspect-plan", "--plan", plan], false).ok).toBe(false);
  value.records[0].derived_local_path = join(root, "library", value.records[0].skill); value.records.push(value.records[0]); await writeFile(plan, JSON.stringify(value));
  expect(run(root, ["state", "inspect-plan", "--plan", plan], false).ok).toBe(false);
});

test("state inspect-plan rejects forged semantic record data without --from", async () => {
  const root = await fixture(); const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  run(root, ["state", "export", "--out", bundle]); run(root, ["state", "stage", "--from", bundle, "--plan", plan]);
  const value = JSON.parse(await readFile(plan, "utf8"));
  value.records[0].skill = "other";
  value.records[0].observed.skill = "other";
  await writeFile(plan, JSON.stringify(value));
  expect(run(root, ["state", "inspect-plan", "--plan", plan], false).ok).toBe(false);
});

test("state inspect-plan validates pure plans without creating target state", async () => {
  const root = await mkdtemp(join(tmpdir(), "skillsync-plan-uninitialized-")); roots.push(root);
  const bundle = join(root, "bundle.json"); const plan = join(root, "plan.json");
  const target = { format: "skillsync-state-stage-plan", version: 1, non_activating: true, bundle_hash: "0".repeat(64), target: { library: join(root, "library"), state_version: 5, initialized: false }, records: [], activation: "not_supported" };
  await writeFile(plan, JSON.stringify(target));
  const result = run(root, ["state", "inspect-plan", "--plan", plan], false);
  expect(result.ok).toBe(false); expect(await Bun.file(join(root, "config", "state.json")).exists()).toBe(false);
});

test("state export is deterministic metadata-only and inspect is read-only", async () => {
  const root = await fixture(); const one = join(root, "one.json"); const two = join(root, "two.json");
  const before = await readFile(join(root, "config", "state.json"), "utf8");
  expect(run(root, ["state", "export", "--out", one]).format).toBe("skillsync-state-metadata");
  expect(run(root, ["state", "export", "--out", two]).format).toBe("skillsync-state-metadata");
  expect(await readFile(one, "utf8")).toBe(await readFile(two, "utf8"));
  expect(await readFile(join(root, "config", "state.json"), "utf8")).toBe(before);
  const inspected = run(root, ["state", "inspect", "--from", one]);
  expect(inspected.metadata_only).toBe(true); expect(inspected.sets.core.members).toEqual(["library:demo"]);
  expect(JSON.stringify(inspected)).not.toContain(root);
});

test("state inspect rejects unknown nested fields and wrong record types in every metadata map", async () => {
  const root = await fixture(); const out = join(root, "bundle.json"); run(root, ["state", "export", "--out", out]);
  for (const kind of ["subscriptions", "publications", "pending_publications", "sets", "local_adoptions"]) {
    for (const record of [{ extra: true }, "wrong", 7, []]) {
      const value = JSON.parse(await readFile(out, "utf8")); value[kind] = { forged: record }; await writeFile(out, JSON.stringify(value));
      expect(run(root, ["state", "inspect", "--from", out], false).ok).toBe(false);
    }
  }
});

test("state inspect rejects tampering, unknown fields, absolute paths, and symlink bundles", async () => {
  const root = await fixture(); const out = join(root, "bundle.json"); run(root, ["state", "export", "--out", out]);
  for (const mutate of [
    (x: any) => { x.extra = true; },
    (x: any) => { x.sets.core.members = ["../escape"]; },
    (x: any) => { x.subscriptions = { bad: { source_path: "/absolute" } }; },
  ]) { const value = JSON.parse(await readFile(out, "utf8")); mutate(value); await writeFile(out, JSON.stringify(value)); expect(run(root, ["state", "inspect", "--from", out], false).ok).toBe(false); }
  const target = join(root, "target"); await writeFile(target, await readFile(out, "utf8")); const link = join(root, "link.json"); await symlink(target, link); expect(run(root, ["state", "inspect", "--from", link], false).ok).toBe(false);
});

test("state inspect rejects credentials in URL and SCP sources or destinations", async () => {
  const root = await fixture(); const out = join(root, "bundle.json"); run(root, ["state", "export", "--out", out]);
  for (const remote of ["https://user:pass@example.com/repo.git", "http://user@example.com/repo.git", "ssh://user@example.com/repo", "git@github.com:org/repo.git"]) {
    const value = JSON.parse(await readFile(out, "utf8")); value.publications = { "pub-forged": { skill: "demo", destination: remote, branch: "main", path: ".", approved: true, status: "synced", last_hash: null, last_sync: 0 } }; await writeFile(out, JSON.stringify(value));
    expect(run(root, ["state", "inspect", "--from", out], false).ok).toBe(false);
  }
});

test("state inspect accepts safe slash-separated Git branches", async () => {
  const root = await fixture(); const out = join(root, "bundle.json"); run(root, ["state", "export", "--out", out]);
  const value = JSON.parse(await readFile(out, "utf8"));
  const publicationKey = "pub-" + createHash("sha256").update(Buffer.from("publication\0demo\0https://github.com/org/repo.git\0feature/foo\0.\0")).digest("hex");
  value.publications = { [publicationKey]: { skill: "demo", destination: "https://github.com/org/repo.git", branch: "feature/foo", path: ".", approved: true, status: "synced", last_hash: null, last_sync: 0 } }; await writeFile(out, JSON.stringify(value));
  expect(run(root, ["state", "inspect", "--from", out]).publications[publicationKey].branch).toBe("feature/foo");
});

test("state inspect rejects ftp destinations even with a correctly computed identity", async () => {
  const root = await fixture(); const out = join(root, "bundle.json"); run(root, ["state", "export", "--out", out]);
  const value = JSON.parse(await readFile(out, "utf8"));
  const publicationKey = "pub-" + createHash("sha256").update(Buffer.from("publication\0demo\0ftp://example.com/repo\0main\0.\0")).digest("hex");
  value.publications = { [publicationKey]: { skill: "demo", destination: "ftp://example.com/repo", branch: "main", path: ".", approved: true, status: "synced", last_hash: null, last_sync: 0 } };
  await writeFile(out, JSON.stringify(value));
  expect(run(root, ["state", "inspect", "--from", out], false).ok).toBe(false);
});

test("state inspect rejects forged map identities and noncanonical source paths", async () => {
  const root = await fixture(); const out = join(root, "bundle.json"); run(root, ["state", "export", "--out", out]);
  const value = JSON.parse(await readFile(out, "utf8")); value.sets = { "bad/name": { members: ["library:demo"] } }; await writeFile(out, JSON.stringify(value)); expect(run(root, ["state", "inspect", "--from", out], false).ok).toBe(false);
  value.subscriptions = { "rel-forged": { skill: "demo", source: "https://github.com/org/repo.git", branch: "main", source_path: "foo/../bar", baseline_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", baseline_source: "", baseline_source_path: "", status: "synced", conflict_selection: null, last_sync: 0, update_count: 0 } }; await writeFile(out, JSON.stringify(value)); expect(run(root, ["state", "inspect", "--from", out], false).ok).toBe(false);
});
