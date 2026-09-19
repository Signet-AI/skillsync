import { expect, test } from "bun:test";
import { mkdir, mkdtemp, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { commandBinary } from "./test_harness";

type Env = Record<string, string>;
const decoder = new TextDecoder();
function env(root: string): Env { return { ...process.env, HOME: join(root, "home"), SKILLSYNC_CONFIG_DIR: join(root, "config"), SKILLSYNC_LIBRARY: join(root, "library"), GIT_CONFIG_GLOBAL: join(root, "gitconfig"), GIT_TERMINAL_PROMPT: "0" } as Env; }
function run(root: string, args: string[], extra: Env = {}) { const r = Bun.spawnSync({ cmd: [commandBinary(extra), "--json", ...args], env: { ...env(root), ...extra }, stdout: "pipe", stderr: "pipe" }); expect(r.exitCode, decoder.decode(r.stderr)).toBe(0); return JSON.parse(decoder.decode(r.stdout)); }
async function fixture() { const root = await mkdtemp(join(tmpdir(), "skillsync-cycle-")); await mkdir(join(root, "home"), { recursive: true }); await writeFile(join(root, "gitconfig"), "[user]\nname = Test\nemail = test@example.invalid\n"); run(root, ["init"]); return root; }

 test("empty initialized cycle reports an additive zero summary", async () => {
  const root = await fixture();
  const result = run(root, ["worker", "--once"]);
  expect(result.results).toEqual([]);
  expect(result.cycle_summary).toEqual({ total_results: 0, subscriptions: { attempted: 0, succeeded: 0, failed: 0, scheduled: 0 }, publications: { attempted: 0, succeeded: 0, failed: 0, scheduled: 0 }, by_status: {} });
 });

test("cycle summary distinguishes subscription success and source failure", async () => {
  const root = await fixture();
  const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions.good = { skill: "good", source: "/missing-source", branch: "main", branch_policy: { kind: "remote_default" }, source_path: ".", baseline_path: join(root, "config/baselines/good"), baseline_hash: "deadbeef", baseline_source: "/missing-source", baseline_source_path: ".", local_path: join(root, "library/good"), status: "synced", recovery_path: null, last_sync: 0, update_count: 0 };
  state.subscriptions.bad = { ...state.subscriptions.good, source: "/missing-source-2", skill: "bad" };
  await writeFile(statePath, JSON.stringify(state));
  const result = run(root, ["worker", "--once"]);
  expect(result.cycle_summary.subscriptions.failed).toBe(2);
  expect(result.cycle_summary.by_status.source_missing ?? result.cycle_summary.by_status.conflict).toBeGreaterThan(0);
 });

test("scheduled publication is not counted as attempted", async () => {
  const root = await fixture(); const statePath = join(root, "config", "state.json"); const state = JSON.parse(await readFile(statePath, "utf8"));
  const key = "pub-pending"; state.pending_publications = { [key]: { publication: { skill: "missing", destination: "https://offline.invalid/repo.git", branch: "main", path: "skills/missing", approved: true, status: "pending_push", last_hash: null, last_sync: 0 }, attempt_count: 1, last_attempt_at: 100, next_attempt_at: 200 } }; await writeFile(statePath, JSON.stringify(state));
  const result = run(root, ["worker", "--once"], { SKILLSYNC_TEST_NOW: "101" });
  expect(result.results[0].queue).toBe("scheduled"); expect(result.cycle_summary.publications.scheduled).toBe(1); expect(result.cycle_summary.publications.attempted).toBe(0); expect(result.cycle_summary.by_status.pending_push).toBe(1);
  expect(JSON.stringify(result)).not.toMatch(/offline\.invalid|SKILLSYNC_TEST_|tmp/);
 });

test("malformed relationship remains isolated and diagnostics are deterministic without state mutation", async () => {
  const root = await fixture(); const statePath = join(root, "config", "state.json"); const before = await readFile(statePath, "utf8"); const state = JSON.parse(before); state.subscriptions["../malformed"] = { skill: "bad", source: "/unreachable", branch: "main", source_path: ".", baseline_path: "/unstable/tmp/path", baseline_hash: "deadbeef", local_path: join(root, "library/bad"), status: "synced", recovery_path: null, last_sync: 0, update_count: 0 }; await writeFile(statePath, JSON.stringify(state));
  const first = run(root, ["worker", "--once"]); const second = run(root, ["worker", "--once"]); expect(first.cycle_summary).toEqual(second.cycle_summary); expect(first.cycle_summary.subscriptions.failed).toBe(1); expect(JSON.stringify(first)).not.toContain("/unstable/tmp/path"); expect(await readFile(statePath, "utf8")).not.toBe(before);
 });
