import { afterEach, expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, readdir, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { childEnv, commandBinary } from "./test_harness";

type Env = Record<string, string>;
type Fixture = { root: string; config: string; library: string; env: Env };
type Result = { code: number; stdout: string; stderr: string; json: any };

const decoder = new TextDecoder();
const fixtureRoots = new Set<string>();

afterEach(async () => {
  for (const root of fixtureRoots) await rm(root, { recursive: true, force: true });
  fixtureRoots.clear();
});

async function put(path: string, contents: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, contents);
}

async function fixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-conflict-resolution-"));
  const home = join(root, "home");
  const config = join(root, "config");
  const library = join(root, "library");
  const gitConfig = join(root, "gitconfig");
  await mkdir(home, { recursive: true });
  await writeFile(gitConfig, "[user]\n\tname = Skillsync Bun Test\n\temail = skillsync@example.invalid\n");
  fixtureRoots.add(root);
  return {
    root,
    config,
    library,
    env: {
      HOME: home,
      SKILLSYNC_CONFIG_DIR: config,
      SKILLSYNC_LIBRARY: library,
      GIT_CONFIG_GLOBAL: gitConfig,
      GIT_TERMINAL_PROMPT: "0",
    },
  };
}

function run(f: Fixture, args: string[], success = true, extra: Env = {}): Result {
  const result = Bun.spawnSync({
    cmd: [commandBinary(extra), ...args],
    env: childEnv({ ...f.env, ...extra }),
    stdout: "pipe",
    stderr: "pipe",
  });
  const stdout = decoder.decode(result.stdout);
  const stderr = decoder.decode(result.stderr);
  expect(result.exitCode, `${args.join(" ")}\nstdout: ${stdout}\nstderr: ${stderr}`).toBe(success ? 0 : 1);
  expect(stdout.length, `empty stdout for ${args.join(" ")}\nstderr: ${stderr}`).toBeGreaterThan(0);
  return { code: result.exitCode, stdout, stderr, json: JSON.parse(stdout) };
}

function git(f: Fixture, cwd: string, args: string[]): void {
  const result = Bun.spawnSync({
    cmd: ["git", ...args],
    cwd,
    env: childEnv(f.env),
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(result.exitCode, decoder.decode(result.stderr)).toBe(0);
}

async function conflictFixture(): Promise<{ f: Fixture; relationship: string; statePath: string; livePath: string }> {
  const f = await fixture();
  const source = join(f.root, "source");
  const upstream = join(f.root, "upstream.git");
  Bun.spawnSync({ cmd: ["git", "init", "--bare", upstream], env: childEnv(f.env), stdout: "pipe", stderr: "pipe" });
  Bun.spawnSync({ cmd: ["git", "init", "-b", "trunk", source], env: childEnv(f.env), stdout: "pipe", stderr: "pipe" });
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nbase\n");
  git(f, source, ["add", "."]);
  git(f, source, ["commit", "-qm", "initial"]);
  git(f, source, ["remote", "add", "origin", upstream]);
  git(f, source, ["push", "-q", "-u", "origin", "trunk"]);

  run(f, ["--json", "init"]);
  run(f, ["--json", "subscribe", upstream, "--skill", "skills/demo"]);
  await put(join(f.library, "demo/SKILL.md"), "name: demo\nlocal change\n");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nincoming change\n");
  git(f, source, ["add", "."]);
  git(f, source, ["commit", "-qm", "conflict"]);
  git(f, source, ["push", "-q"]);
  const update = run(f, ["--json", "update"]).json;
  expect(update.results.some((item: any) => item.status === "conflict")).toBe(true);

  const statePath = join(f.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  const relationship = Object.keys(state.subscriptions)[0]!;
  return { f, relationship, statePath, livePath: join(f.library, "demo/SKILL.md") };
}

test("explicit conflict selection resumes incoming and retains immutable evidence", async () => {
  const { f, relationship, statePath, livePath } = await conflictFixture();
  const recoveryBefore = await readdir(join(f.config, "recovery"));
  const selected = run(f, ["--json", "conflicts", "resolve", relationship, "--incoming"]).json;
  expect(selected.status).toBe("selected");
  expect(selected.selected_side).toBe("incoming");
  expect(JSON.parse(await readFile(statePath, "utf8")).subscriptions[relationship].conflict_selection).toBe("incoming");

  const resumed = run(f, ["--json", "conflicts", "resume", relationship]).json;
  expect(resumed.status).toBe("synced");
  expect(resumed.selected_side).toBe("incoming");
  expect(resumed.recovery_retained).toBe(true);
  expect(await readFile(livePath, "utf8")).toBe("name: demo\nincoming change\n");

  const afterState = JSON.parse(await readFile(statePath, "utf8"));
  expect(await readFile(afterState.subscriptions[relationship].baseline_path + "/SKILL.md", "utf8")).toBe(
    "name: demo\nincoming change\n",
  );
  expect(await readdir(join(f.config, "recovery"))).toEqual(recoveryBefore);
  expect(run(f, ["--json", "conflicts", "list"]).json.count).toBe(0);

  const repeated = run(f, ["--json", "conflicts", "resume", relationship]).json;
  expect(repeated.status).toBe("synced");
  expect(repeated.recovery_retained).toBe(true);
  expect(await readFile(livePath, "utf8")).toBe("name: demo\nincoming change\n");
});

test("rejects a tampered current-version conflict selection on repeated resume", async () => {
  const { f, relationship, statePath } = await conflictFixture();
  run(f, ["--json", "conflicts", "resolve", relationship, "--incoming"]);
  run(f, ["--json", "conflicts", "resume", relationship]);
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions[relationship].conflict_selection = "unexpected";
  await writeFile(statePath, JSON.stringify(state));

  const rejected = run(f, ["--json", "conflicts", "resume", relationship], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("invalid conflict side selection");
});

test("stale live content blocks conflict selection without mutation", async () => {
  const { f, relationship, statePath, livePath } = await conflictFixture();
  const beforeState = await readFile(statePath, "utf8");
  const beforeRecovery = await readdir(join(f.config, "recovery"));
  await put(livePath, "name: demo\nnewer user edit\n");

  const rejected = run(f, ["--json", "conflicts", "resolve", relationship, "--local"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("live content changed");
  expect(await readFile(statePath, "utf8")).toBe(beforeState);
  expect(await readdir(join(f.config, "recovery"))).toEqual(beforeRecovery);
  expect(await readFile(livePath, "utf8")).toBe("name: demo\nnewer user edit\n");
});

test("migrates pre-v5 baseline source identity for conflict inspection", async () => {
  const { f, relationship, statePath } = await conflictFixture();
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.version = 4;
  delete state.subscriptions[relationship].baseline_source;
  delete state.subscriptions[relationship].baseline_source_path;
  await writeFile(statePath, JSON.stringify(state));

  const shown = run(f, ["--json", "conflicts", "show", relationship]).json;
  expect(shown.status).toBe("conflict");
  expect(shown.baseline_source).toBe(state.subscriptions[relationship].source);
  expect(shown.baseline_source_path).toBe(state.subscriptions[relationship].source_path);
});

test("resume rolls back live and baseline on state-save failure", async () => {
  const { f, relationship, statePath, livePath } = await conflictFixture();
  run(f, ["--json", "conflicts", "resolve", relationship, "--incoming"]);
  const beforeState = await readFile(statePath, "utf8");
  const state = JSON.parse(beforeState);
  const baselinePath = state.subscriptions[relationship].baseline_path;
  const beforeBaseline = await readFile(join(baselinePath, "SKILL.md"), "utf8");
  const beforeRecovery = await readdir(join(f.config, "recovery"));

  const failed = run(
    f,
    ["--json", "conflicts", "resume", relationship],
    false,
    { SKILLSYNC_TEST_FAIL_STATE_SAVE: "1" },
  );
  expect(failed.json.ok).toBe(false);
  expect(failed.json.message).toContain("replacements rolled back");
  expect(await readFile(statePath, "utf8")).toBe(beforeState);
  expect(await readFile(livePath, "utf8")).toBe("name: demo\nlocal change\n");
  expect(await readFile(join(baselinePath, "SKILL.md"), "utf8")).toBe(beforeBaseline);
  expect(await readdir(join(f.config, "recovery"))).toEqual(beforeRecovery);
});

test("resume rolls back both replacements when the second commit preparation fails", async () => {
  const { f, relationship, statePath, livePath } = await conflictFixture();
  run(f, ["--json", "conflicts", "resolve", relationship, "--incoming"]);
  const beforeState = await readFile(statePath, "utf8");
  const state = JSON.parse(beforeState);
  const baselinePath = state.subscriptions[relationship].baseline_path;
  const beforeBaseline = await readFile(join(baselinePath, "SKILL.md"), "utf8");

  const failed = run(
    f,
    ["--json", "conflicts", "resume", relationship],
    false,
    { SKILLSYNC_TEST_FAIL_REPLACEMENT_COMMIT: "1" },
  );
  expect(failed.json.ok).toBe(false);
  expect(failed.json.message).toContain("injected replacement commit failure");
  expect(await readFile(statePath, "utf8")).toBe(beforeState);
  expect(await readFile(livePath, "utf8")).toBe("name: demo\nlocal change\n");
  expect(await readFile(join(baselinePath, "SKILL.md"), "utf8")).toBe(beforeBaseline);
});
