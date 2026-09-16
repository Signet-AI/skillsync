import { expect, test } from "bun:test";
import { mkdtemp, mkdir, readFile, readdir, symlink, unlink, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

type Env = Record<string, string>;
type Result = { code: number; stdout: string; stderr: string };

type Fixture = {
  root: string;
  config: string;
  library: string;
  env: Env;
};

const binary = process.env.SKILLSYNC_BIN ?? resolve(import.meta.dir, "../target/debug/skillsync");
const decoder = new TextDecoder();

function inheritedEnv(): Env {
  return Object.fromEntries(
    Object.entries(process.env).filter((entry): entry is [string, string] => entry[1] !== undefined),
  );
}

function run(program: string, args: string[], cwd: string | undefined, env: Env): Result {
  const result = Bun.spawnSync({
    cmd: [program, ...args],
    cwd,
    env: { ...inheritedEnv(), ...env },
    stdout: "pipe",
    stderr: "pipe",
  });
  return {
    code: result.exitCode,
    stdout: decoder.decode(result.stdout),
    stderr: decoder.decode(result.stderr),
  };
}

function checked(result: Result, label: string): Result {
  expect(result.code, `${label}\nstderr: ${result.stderr}`).toBe(0);
  return result;
}

async function put(path: string, contents: string): Promise<void> {
  await mkdir(dirname(path), { recursive: true });
  await writeFile(path, contents);
}

async function makeFixture(): Promise<Fixture> {
  const root = await mkdtemp(join(tmpdir(), "skillsync-bun-"));
  const home = join(root, "home");
  const config = join(root, "config");
  const library = join(root, "library");
  await mkdir(home, { recursive: true });
  const gitConfig = join(root, "gitconfig");
  await writeFile(
    gitConfig,
    "[user]\n\tname = Skillsync Bun Test\n\temail = skillsync@example.invalid\n",
  );
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

function git(fixture: Fixture, cwd: string, args: string[]): Result {
  return checked(run("git", args, cwd, fixture.env), `git ${args.join(" ")}`);
}

function skillsync(fixture: Fixture, args: string[], success = true): { result: Result; json: any } {
  const result = run(binary, args, undefined, fixture.env);
  expect(result.code, `skillsync ${args.join(" ")}\nstderr: ${result.stderr}`).toBe(success ? 0 : 1);
  expect(result.stdout.length, `skillsync stdout was empty: ${result.stderr}`).toBeGreaterThan(0);
  return { result, json: JSON.parse(result.stdout) };
}

function countPublications(status: any): number {
  return Object.keys(status.publications ?? {}).length;
}

test("local Git subscribe, merge, conflict recovery, and scoped publication", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  const upstream = join(fixture.root, "upstream.git");

  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init source remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init source");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nbase\n");
  await put(join(source, "skills/demo/references/one.md"), "one\n");
  await put(join(source, "skills/demo/references/child/SKILL.md"), "name: child\n");
  await put(join(source, "unrelated.txt"), "unrelated\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "initial"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);

  expect(skillsync(fixture, ["--json", "init"]).json.ok).toBe(true);
  const subscribed = skillsync(fixture, ["--json", "subscribe", upstream, "--skill", "skills/demo"]).json;
  expect(subscribed.skill).toBe("demo");
  expect(await readFile(join(fixture.library, "demo/SKILL.md"), "utf8")).toBe("name: demo\nbase\n");
  expect(await readFile(join(fixture.library, "demo/references/child/SKILL.md"), "utf8")).toBe(
    "name: child\n",
  );
  expect(await Bun.file(join(fixture.library, "unrelated.txt")).exists()).toBe(false);

  const unchanged = skillsync(fixture, ["--json", "update"]).json;
  expect(unchanged.results.some((item: any) => item.status === "synced")).toBe(true);

  await put(join(fixture.library, "demo/references/one.md"), "local learning\n");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nupstream\n");
  await put(join(source, "skills/demo/references/newdir/new.md"), "nested\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "upstream change"]);
  git(fixture, source, ["push"]);

  const merged = skillsync(fixture, ["--json", "update"]).json;
  expect(merged.results.some((item: any) => item.skill === "demo" && item.status === "synced")).toBe(true);
  expect(await readFile(join(fixture.library, "demo/SKILL.md"), "utf8")).toBe("name: demo\nupstream\n");
  expect(await readFile(join(fixture.library, "demo/references/one.md"), "utf8")).toBe("local learning\n");
  expect(await readFile(join(fixture.library, "demo/references/newdir/new.md"), "utf8")).toBe("nested\n");

  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\nlocal conflict\n");
  await put(join(source, "skills/demo/SKILL.md"), "name: demo\nremote conflict\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "conflict"]);
  git(fixture, source, ["push"]);
  const conflict = skillsync(fixture, ["--json", "update"]).json;
  expect(conflict.results.some((item: any) => item.skill === "demo" && item.status === "conflict")).toBe(true);
  const live = await readFile(join(fixture.library, "demo/SKILL.md"), "utf8");
  expect(live).toContain("local conflict");
  expect(live).not.toContain("<<<<<<<");
  expect((await readdir(join(fixture.config, "recovery"))).length).toBe(1);

  const destination = join(fixture.root, "destination.git");
  const seed = join(fixture.root, "destination-seed");
  checked(run("git", ["init", "--bare", destination], undefined, fixture.env), "init destination remote");
  checked(run("git", ["init", "-b", "trunk", seed], undefined, fixture.env), "init destination");
  await put(join(seed, "README.md"), "keep\n");
  await put(join(seed, "skills/publishable/.env"), "SECRET=keep\n");
  git(fixture, seed, ["add", "."]);
  git(fixture, seed, ["commit", "-m", "seed"]);
  git(fixture, seed, ["remote", "add", "origin", destination]);
  git(fixture, seed, ["push", "-u", "origin", "trunk"]);

  await put(join(fixture.library, "publishable/SKILL.md"), "name: publishable\nv1\n");
  await put(join(fixture.library, "publishable/references/old.md"), "old\n");
  const published = skillsync(fixture, ["--json", "publish", "publishable", "--repo", destination, "--yes"]).json;
  expect(published.status).toBe("published");
  const firstLog = git(fixture, seed, ["ls-remote", destination, "refs/heads/trunk"]).stdout;
  const destinationClone = join(fixture.root, "published-clone");
  checked(
    run("git", ["clone", "--quiet", "--branch", "trunk", destination, destinationClone], undefined, fixture.env),
    "clone published destination",
  );
  expect(await readFile(join(destinationClone, "README.md"), "utf8")).toBe("keep\n");
  expect(await readFile(join(destinationClone, "skills/publishable/.env"), "utf8")).toBe("SECRET=keep\n");
  expect(await readFile(join(destinationClone, "skills/publishable/references/old.md"), "utf8")).toBe("old\n");

  const repeated = skillsync(fixture, ["--json", "publish", "publishable", "--repo", destination, "--yes"]).json;
  expect(repeated.status).toBe("published");
  expect(git(fixture, seed, ["ls-remote", destination, "refs/heads/trunk"]).stdout).toBe(firstLog);

  await put(join(fixture.library, "publishable/SKILL.md"), "name: publishable\nv2\n");
  const synced = skillsync(fixture, ["--json", "sync"]).json;
  expect(synced.results.some((item: any) => item.skill === "publishable" && item.status === "published")).toBe(true);
  const updatedClone = join(fixture.root, "updated-clone");
  checked(
    run("git", ["clone", "--quiet", "--branch", "trunk", destination, updatedClone], undefined, fixture.env),
    "clone updated destination",
  );
  expect(await readFile(join(updatedClone, "skills/publishable/SKILL.md"), "utf8")).toBe(
    "name: publishable\nv2\n",
  );
  expect(await readFile(join(updatedClone, "skills/publishable/.env"), "utf8")).toBe("SECRET=keep\n");

  const secondDestination = join(fixture.root, "second-destination.git");
  checked(run("git", ["init", "--bare", secondDestination], undefined, fixture.env), "init second destination");
  skillsync(fixture, ["--json", "publish", "publishable", "--repo", secondDestination, "--yes"]);
  const statusWithTwo = skillsync(fixture, ["--json", "status"]).json;
  expect(countPublications(statusWithTwo)).toBe(2);
  skillsync(fixture, ["--json", "unpublish", "publishable", "--repo", destination]);
  const statusWithOne = skillsync(fixture, ["--json", "status"]).json;
  expect(countPublications(statusWithOne)).toBe(1);

  const unsubscribed = skillsync(fixture, ["--json", "unsubscribe", "demo"]).json;
  expect(unsubscribed.status).toBe("unsubscribed");
  expect(await Bun.file(join(fixture.library, "demo/SKILL.md")).exists()).toBe(true);
  const unpublished = skillsync(fixture, ["--json", "unpublish", "publishable", "--repo", secondDestination]).json;
  expect(unpublished.status).toBe("unpublished; destination retained");
  expect(await Bun.file(join(fixture.library, "publishable/SKILL.md")).exists()).toBe(true);

  const failed = skillsync(fixture, ["--json", "subscribe", upstream], false);
  expect(failed.json.ok).toBe(false);
});

test("tampered persisted paths are rejected without reading outside state", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions["../escape"] = {
    skill: "tampered",
    source: "/unreachable",
    branch: "trunk",
    source_path: ".",
    baseline_path: join(fixture.root, "outside-baseline"),
    baseline_hash: "deadbeef",
    local_path: join(fixture.library, "tampered"),
    status: "synced",
    recovery_path: null,
    last_sync: 0,
    update_count: 0,
  };
  await writeFile(statePath, JSON.stringify(state));
  const failed = skillsync(fixture, ["--json", "update"], false);
  expect(failed.json.ok).toBe(false);
  expect(failed.json.message).toContain("relationship key");
  expect(await Bun.file(join(fixture.root, "outside-baseline")).exists()).toBe(false);
});

test("rejects a persisted library redirect", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.library = join(fixture.root, "outside-library");
  await writeFile(statePath, JSON.stringify(state));
  const failed = skillsync(fixture, ["--json", "status"], false);
  expect(failed.json.ok).toBe(false);
  expect(failed.json.message).toContain("persisted library");
  expect(await Bun.file(join(fixture.root, "outside-library")).exists()).toBe(false);
});

test("rejects symlinked persisted config and state files", async () => {
  if (process.platform === "win32") return;
  const configFixture = await makeFixture();
  skillsync(configFixture, ["--json", "init"]);
  const externalConfig = join(configFixture.root, "external-config.toml");
  await writeFile(externalConfig, `library = ${JSON.stringify(configFixture.library)}\n`);
  const configPath = join(configFixture.config, "config.toml");
  await unlink(configPath);
  await symlink(externalConfig, configPath);
  const configFailure = skillsync(configFixture, ["--json", "status"], false);
  expect(configFailure.json.message).toContain("symlink config");

  const stateFixture = await makeFixture();
  skillsync(stateFixture, ["--json", "init"]);
  const externalState = join(stateFixture.root, "external-state.json");
  await writeFile(externalState, "{}");
  const statePath = join(stateFixture.config, "state.json");
  await unlink(statePath);
  await symlink(externalState, statePath);
  const stateFailure = skillsync(stateFixture, ["--json", "status"], false);
  expect(stateFailure.json.message).toContain("symlink state");
});

test("rejects symlinked package content without copying it", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "source");
  const upstream = join(fixture.root, "upstream.git");
  const outside = join(fixture.root, "outside.txt");
  await writeFile(outside, "must not be copied\n");
  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init unsafe remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init unsafe source");
  await put(join(source, "skills/unsafe/SKILL.md"), "name: unsafe\n");
  await mkdir(join(source, "skills/unsafe/references"), { recursive: true });
  await symlink(outside, join(source, "skills/unsafe/references/outside.txt"));
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "unsafe package"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);
  skillsync(fixture, ["--json", "init"]);
  const failed = skillsync(
    fixture,
    ["--json", "subscribe", upstream, "--skill", "unsafe"],
    false,
  );
  expect(failed.json.ok).toBe(false);
  expect(await Bun.file(join(fixture.library, "unsafe/SKILL.md")).exists()).toBe(false);
  expect(await readFile(outside, "utf8")).toBe("must not be copied\n");
});
