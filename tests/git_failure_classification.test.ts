import { expect, test, afterEach } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";
import { join } from "node:path";
import { tmpdir } from "node:os";

const binary = process.env.SKILLSYNC_BIN ?? join(import.meta.dir, "../target/debug/skillsync");
const dec = new TextDecoder();
const roots: string[] = [];
function env(root: string) { return { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library"), GIT_AUTHOR_NAME: "Skillsync Test", GIT_AUTHOR_EMAIL: "test@example.invalid", GIT_COMMITTER_NAME: "Skillsync Test", GIT_COMMITTER_EMAIL: "test@example.invalid" }; }
function run(root: string, args: string[], ok = true) {
  const r = Bun.spawnSync({ cmd: [binary, "--json", ...args], env: env(root), stdout: "pipe", stderr: "pipe" });
  expect(r.exitCode, dec.decode(r.stderr)).toBe(ok ? 0 : 1);
  return JSON.parse(dec.decode(r.stdout));
}
function git(cwd: string, args: string[]) {
  const r = Bun.spawnSync({ cmd: ["git", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", ...args], cwd, stdout: "pipe", stderr: "pipe" });
  expect(r.exitCode, dec.decode(r.stderr)).toBe(0);
}
async function repo(root: string) {
  const source = join(root, "source"); await mkdir(source, { recursive: true });
  git(source, ["init", "-qb", "main"]); await mkdir(join(source, "nested", "demo"), { recursive: true });
  await writeFile(join(source, "nested", "demo", "SKILL.md"), "name: demo\noriginal\n"); git(source, ["add", "."]); git(source, ["commit", "-qm", "initial"]); return source;
}
async function fixture() { const root = await mkdtemp(join(tmpdir(), "skillsync-git-failure-")); roots.push(root); const source = await repo(root); run(root, ["init"]); run(root, ["subscribe", source, "--skill", "nested/demo"]); return { root, source }; }
afterEach(async () => { await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true }))); });

test("worker classifies a missing repository without mutating live package or baseline", async () => {
  const { root, source } = await fixture(); const stateBefore = JSON.parse(await readFile(join(root, "config", "state.json"), "utf8"));
  const live = await readFile(join(root, "library", "demo", "SKILL.md"), "utf8"); const key = Object.keys(stateBefore.subscriptions)[0]; const baseline = stateBefore.subscriptions[key].baseline_hash;
  await rm(source, { recursive: true, force: true });
  const result = run(root, ["worker", "--once"]); expect(result.results[0].status).toBe("source_missing");
  const stateAfter = JSON.parse(await readFile(join(root, "config", "state.json"), "utf8"));
  expect(stateAfter.subscriptions[key].status).toBe("source_missing"); expect(stateAfter.subscriptions[key].baseline_hash).toBe(baseline);
  expect(await readFile(join(root, "library", "demo", "SKILL.md"), "utf8")).toBe(live);
});

test("successful update reports recovery from an exact persisted source failure and worker forwards it", async () => {
  const { root, source } = await fixture(); const statePath = join(root, "config", "state.json");
  await rm(source, { recursive: true, force: true });
  const failed = run(root, ["worker", "--once"]); expect(failed.results[0]).not.toHaveProperty("recovered_from");
  const missingState = JSON.parse(await readFile(statePath, "utf8")); const key = Object.keys(missingState.subscriptions)[0];
  await repo(root); // recreate the exact source path with the original package
  const recovered = run(root, ["worker", "--once"]);
  expect(recovered.results[0]).toMatchObject({ status: "synced", recovered_from: "source_missing" });
  expect(run(root, ["worker", "--once"]).results[0]).not.toHaveProperty("recovered_from");
  const wrong = JSON.parse(await readFile(statePath, "utf8")); wrong.subscriptions[key].status = "conflict"; await writeFile(statePath, JSON.stringify(wrong));
  expect(run(root, ["worker", "--once"]).results[0]).not.toHaveProperty("recovered_from");
});

test("explicit policy mismatch fails closed before update and never follows mutable branch", async () => {
  const { root, source } = await fixture(); const statePath = join(root, "config", "state.json"); const before = JSON.parse(await readFile(statePath, "utf8")); const key = Object.keys(before.subscriptions)[0];
  git(source, ["checkout", "-qb", "release"]); await writeFile(join(source, "nested", "demo", "SKILL.md"), "name: demo\nrelease\n"); git(source, ["add", "."]); git(source, ["commit", "-qm", "release change"]);
  const state = { ...before, subscriptions: { ...before.subscriptions, [key]: { ...before.subscriptions[key], branch: "main", branch_policy: { kind: "explicit", name: "release" } } } };
  await writeFile(statePath, JSON.stringify(state));
  const result = run(root, ["worker", "--once"], false); expect(JSON.stringify(result)).toContain("branch policy");
  const after = JSON.parse(await readFile(statePath, "utf8")); expect(after.subscriptions[key]).toEqual(state.subscriptions[key]);
  expect(await readFile(join(root, "library", "demo", "SKILL.md"), "utf8")).toBe("name: demo\noriginal\n");
});

test("worker distinguishes missing tracked branch and missing nested package", async () => {
  const { root, source } = await fixture(); const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8")); const key = Object.keys(state.subscriptions)[0];
  state.subscriptions[key].branch = "does-not-exist"; state.subscriptions[key].branch_policy = { kind: "explicit", name: "does-not-exist" }; await writeFile(statePath, JSON.stringify(state));
  expect(run(root, ["worker", "--once"]).results[0].status).toBe("branch_missing");
  state.subscriptions[key].branch = "main"; state.subscriptions[key].branch_policy = { kind: "explicit", name: "main" }; await writeFile(statePath, JSON.stringify(state)); await rm(join(source, "nested", "demo"), { recursive: true, force: true }); git(source, ["add", "-A"]); git(source, ["commit", "-qm", "remove package"]);
  const result = run(root, ["worker", "--once"]); expect(result.results[0].status).toBe("package_missing");
});

test("worker preserves authentication, offline, and permission classifications without leaking diagnostics", async () => {
  const cases = [
    ["https://user:secret@example.invalid/repo.git", "authentication_required"],
    ["https://offline.invalid/repo.git", "offline"],
    ["/root/permission-denied/repo", "permission_denied"],
    ["/tmp/diagnostic@host/repo", "source_missing"],
  ] as const;
  for (const [source, status] of cases) {
    const { root } = await fixture();
    const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
    const key = Object.keys(state.subscriptions)[0]; state.subscriptions[key].source = source;
    await writeFile(statePath, JSON.stringify(state));
    const result = run(root, ["worker", "--once"]); const item = result.results.find((x: any) => x.relationship === key);
    expect(item.status).toBe(status); expect(JSON.stringify(item)).not.toContain("secret"); expect(JSON.stringify(item)).not.toContain("/root/forbidden");
  }
});

test("failure keeps prior provenance and does not auto-adopt a reappeared package", async () => {
  const { root, source } = await fixture(); const statePath = join(root, "config", "state.json"); const before = JSON.parse(await readFile(statePath, "utf8")); const key = Object.keys(before.subscriptions)[0];
  await rm(join(source, "nested", "demo"), { recursive: true, force: true }); git(source, ["add", "-A"]); git(source, ["commit", "-qm", "remove package"]);
  expect(run(root, ["worker", "--once"]).results[0].status).toBe("package_missing");
  await mkdir(join(source, "nested", "demo"), { recursive: true }); await writeFile(join(source, "nested", "demo", "SKILL.md"), "name: demo\nreappeared\n"); git(source, ["add", "."]); git(source, ["commit", "-qm", "reappear"]);
  const after = JSON.parse(await readFile(statePath, "utf8")); expect(after.subscriptions[key].resolved_commit).toBe(before.subscriptions[key].resolved_commit); expect(after.subscriptions[key].status).toBe("package_missing");
 });

 test("failed real publication push is durably retried once when due", async () => {
 const root = await mkdtemp(join(tmpdir(), "skillsync-real-publication-retry-")); roots.push(root);
 const remote = join(root, "remote.git"); git(root, ["init", "--bare", remote]);
 run(root, ["init"]);
 await mkdir(join(root, "library", "demo"), { recursive: true });
 await writeFile(join(root, "library", "demo", "SKILL.md"), "name: demo\ndescription: A valid multiline skill fixture.\n\n# Demo\n\nDo the thing.\n");
 const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
 const publication = { skill: "demo", destination: remote, branch: "main", path: "skills/demo", approved: true, status: "pending_push", last_hash: null, last_sync: 0 };
 const key = "pub-" + createHash("sha256").update(Buffer.from("publication\0" + "demo\0" + remote + "\0main\0skills/demo\0")).digest("hex"); state.pending_publications = { [key]: { publication } }; await writeFile(statePath, JSON.stringify(state));
 const hookBinary = join(import.meta.dir, "../target/test-hooks/debug/skillsync");
 const first = Bun.spawnSync({ cmd: [hookBinary, "--json", "worker", "--once"], env: { ...env(root), SKILLSYNC_TEST_FAIL_PUSH_ONCE: "demo", SKILLSYNC_TEST_NOW: "100" }, stdout: "pipe", stderr: "pipe" });
 expect(first.exitCode).toBe(0); const firstResult = JSON.parse(dec.decode(first.stdout)); expect(firstResult.results[0].status).toBe("conflict");
 const failed = JSON.parse(await readFile(statePath, "utf8")); expect(failed.pending_publications[key].attempt_count).toBe(1); expect(failed.pending_publications[key].next_attempt_at).toBeGreaterThan(100);
 const immediate = Bun.spawnSync({ cmd: [hookBinary, "--json", "worker", "--once"], env: { ...env(root), SKILLSYNC_TEST_NOW: "101" }, stdout: "pipe", stderr: "pipe" });
 expect(immediate.exitCode).toBe(0); const immediateResult = JSON.parse(dec.decode(immediate.stdout)); expect(immediateResult.results[0].queue).toBe("scheduled");
 const unchanged = JSON.parse(await readFile(statePath, "utf8")); expect(unchanged.pending_publications[key].attempt_count).toBe(1);
 const later = Bun.spawnSync({ cmd: [hookBinary, "--json", "worker", "--once"], env: { ...env(root), SKILLSYNC_TEST_NOW: "100000" }, stdout: "pipe", stderr: "pipe" });
 expect(later.exitCode).toBe(0); const laterResult = JSON.parse(dec.decode(later.stdout)); expect(laterResult.results.some((x: any) => x.status === "published")).toBe(true);
 const final = JSON.parse(await readFile(statePath, "utf8")); expect(final.pending_publications[key]).toBeUndefined(); expect(final.publications[key].status).toBe("synced");
 const commits = Bun.spawnSync({ cmd: ["git", "--git-dir", remote, "rev-list", "--count", "main"], stdout: "pipe" }); expect(dec.decode(commits.stdout).trim()).toBe("1");
});

test("failed pending publication exposes durable retry queue metadata", async () => {
 const root = await mkdtemp(join(tmpdir(), "skillsync-publication-retry-red-")); roots.push(root);
 run(root, ["init"]);
 const statePath = join(root, "config", "state.json");
 const state = JSON.parse(await readFile(statePath, "utf8"));
 const publication = { skill: "demo", destination: "https://offline.invalid/repo.git", branch: "main", path: "skills/demo", approved: true, status: "pending_push", last_hash: null, last_sync: 0 };
 const key = "pub-demo";
 state.pending_publications = { [key]: { publication } };
 await writeFile(statePath, JSON.stringify(state));
  const result = run(root, ["worker", "--once"]);
  expect(result.results[0].status).toBe("package_missing");
  const after = JSON.parse(await readFile(statePath, "utf8"));
  expect(after.pending_publications[key].attempt_count).toBe(1);
  expect(after.pending_publications[key].next_attempt_at).toBeGreaterThan(0);
  const skipped = run(root, ["worker", "--once"]);
  expect(skipped.results[0].queue).toBe("scheduled");
  const unchanged = JSON.parse(await readFile(statePath, "utf8"));
  expect(unchanged.pending_publications[key].attempt_count).toBe(1);
});
