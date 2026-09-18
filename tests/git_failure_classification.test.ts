import { expect, test, afterEach } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";

const binary = process.env.SKILLSYNC_BIN ?? join(import.meta.dir, "../target/debug/skillsync");
const dec = new TextDecoder();
const roots: string[] = [];
function env(root: string) { return { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library") }; }
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

test("worker distinguishes missing tracked branch and missing nested package", async () => {
  const { root, source } = await fixture(); const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8")); const key = Object.keys(state.subscriptions)[0];
  state.subscriptions[key].branch = "does-not-exist"; await writeFile(statePath, JSON.stringify(state));
  expect(run(root, ["worker", "--once"]).results[0].status).toBe("branch_missing");
  state.subscriptions[key].branch = "main"; await writeFile(statePath, JSON.stringify(state)); await rm(join(source, "nested", "demo"), { recursive: true, force: true }); git(source, ["add", "-A"]); git(source, ["commit", "-qm", "remove package"]);
  const result = run(root, ["worker", "--once"]); expect(result.results[0].status).toBe("package_missing");
});

test("worker preserves authentication, offline, and permission classifications without leaking diagnostics", async () => {
  const cases = [
    ["https://user:secret@example.invalid/repo.git", "authentication_required"],
    ["https://offline.invalid/repo.git", "offline"],
    ["/root/permission-denied/repo", "permission_denied"],
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
