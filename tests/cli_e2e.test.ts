import { expect, test } from "bun:test";
import { chmod, lstat, mkdtemp, mkdir, readFile, readdir, rm, stat, symlink, unlink, writeFile } from "node:fs/promises";
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
  const workerOnce = skillsync(fixture, ["--json", "worker", "--once"]).json;
  expect(workerOnce.worker).toBe("completed");
  expect(workerOnce.results).toEqual([]);
  expect(skillsync(fixture, ["--json", "status"]).json.worker).toBe("stopped");
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

test("foreground worker owns the state lock and status reflects live ownership", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);

  const worker = Bun.spawn({
    cmd: [binary, "--json", "worker", "--interval", "30"],
    env: { ...inheritedEnv(), ...fixture.env },
    stdout: "pipe",
    stderr: "pipe",
  });

  try {
    let running = false;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const status = skillsync(fixture, ["--json", "status"]).json;
      if (status.worker === "running") {
        running = true;
        break;
      }
      await Bun.sleep(20);
    }
    expect(running).toBe(true);

    const blocked = skillsync(fixture, ["--json", "unsubscribe", "missing"], false);
    expect(blocked.json.ok).toBe(false);
    expect(blocked.json.message).toContain("state is busy");
  } finally {
    worker.kill();
    await worker.exited;
  }

  expect(skillsync(fixture, ["--json", "status"]).json.worker).toBe("stopped");
});

test("worker once isolates malformed relationships instead of aborting the cycle", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  for (const key of ["../bad-one", "../bad-two"]) {
    state.subscriptions[key] = {
      skill: "bad",
      source: "/unreachable",
      branch: "trunk",
      source_path: ".",
      baseline_path: join(fixture.config, "baselines", key),
      baseline_hash: "deadbeef",
      local_path: join(fixture.library, "bad"),
      status: "synced",
      recovery_path: null,
      last_sync: 0,
      update_count: 0,
    };
  }
  await writeFile(statePath, JSON.stringify(state));

  const once = skillsync(fixture, ["--json", "worker", "--once"]).json;
  expect(once.worker).toBe("completed");
  expect(once.results).toHaveLength(2);
  expect(once.results.every((item: any) => item.status === "conflict")).toBe(true);
});

test("worker Ctrl-C terminates a blocked Git subprocess and releases ownership", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  await put(join(fixture.config, "baselines/fixture-rel/SKILL.md"), "name: demo\n");
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.subscriptions["fixture-rel"] = {
    skill: "demo",
    source: "blocked-source",
    branch: "trunk",
    source_path: ".",
    baseline_path: join(fixture.config, "baselines/fixture-rel"),
    baseline_hash: "deadbeef",
    local_path: join(fixture.library, "demo"),
    status: "synced",
    recovery_path: null,
    last_sync: 0,
    update_count: 0,
  };
  await writeFile(statePath, JSON.stringify(state));

  const fakeBin = join(fixture.root, "fake-bin");
  await mkdir(fakeBin, { recursive: true });
  const fakeGit = join(fakeBin, "git");
  await writeFile(fakeGit, "#!/bin/sh\nsleep 30\n");
  await chmod(fakeGit, 0o755);
  fixture.env.PATH = `${fakeBin}:${inheritedEnv().PATH ?? ""}`;

  const worker = Bun.spawn({
    cmd: [binary, "--json", "worker", "--interval", "30"],
    env: { ...inheritedEnv(), ...fixture.env },
    stdout: "pipe",
    stderr: "pipe",
  });
  try {
    let running = false;
    for (let attempt = 0; attempt < 100; attempt += 1) {
      const status = skillsync(fixture, ["--json", "status"]).json;
      if (status.worker === "running") {
        running = true;
        break;
      }
      await Bun.sleep(20);
    }
    expect(running).toBe(true);
    worker.kill("SIGINT");
    const exitCode = await Promise.race([
      worker.exited,
      Bun.sleep(3000).then(() => null),
    ]);
    expect(exitCode).not.toBeNull();
    const output = await new Response(worker.stdout).text();
    const result = JSON.parse(output);
    expect(result.worker).toBe("stopped");
    expect(result.cancelled).toBe(true);
  } finally {
    worker.kill();
    await worker.exited;
  }
  expect(skillsync(fixture, ["--json", "status"]).json.worker).toBe("stopped");
});

test("preserves executable mode on upstream package additions", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  const source = join(fixture.root, "mode-source");
  const upstream = join(fixture.root, "mode-upstream.git");
  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init mode remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init mode source");
  await put(join(source, "skills/mode/SKILL.md"), "name: mode\nbase\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "initial mode package"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "subscribe", upstream, "--skill", "mode"]);

  const script = join(source, "skills/mode/scripts/check.sh");
  await put(script, "#!/bin/sh\nexit 0\n");
  await chmod(script, 0o755);
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "add executable"]);
  git(fixture, source, ["push"]);
  skillsync(fixture, ["--json", "update"]);

  const installed = await stat(join(fixture.library, "mode/scripts/check.sh"));
  expect(installed.mode & 0o111).toBe(0o111);
});

test("persists publication intent before a failed push and retries it", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  await put(join(fixture.library, "journal/SKILL.md"), "name: journal\nv1\n");
  const destination = join(fixture.root, "journal-destination.git");
  checked(run("git", ["init", "--bare", destination], undefined, fixture.env), "init journal destination");
  const updateHook = join(destination, "hooks/update");
  await writeFile(updateHook, "#!/bin/sh\nexit 1\n");
  await chmod(updateHook, 0o755);

  const failed = skillsync(
    fixture,
    ["--json", "publish", "journal", "--repo", destination, "--yes"],
    false,
  );
  expect(failed.json.ok).toBe(false);
  const pending = skillsync(fixture, ["--json", "status"]).json;
  expect(Object.keys(pending.pending_publications ?? {})).toHaveLength(1);

  await unlink(updateHook);
  const retried = skillsync(fixture, ["--json", "sync"]).json;
  expect(retried.results.some((item: any) => item.skill === "journal" && item.status === "published")).toBe(true);
  const recovered = skillsync(fixture, ["--json", "status"]).json;
  expect(Object.keys(recovered.pending_publications ?? {})).toHaveLength(0);
  expect(countPublications(recovered)).toBe(1);
});

test("honors JSON mode for clap validation errors", async () => {
  const fixture = await makeFixture();
  const result = run(binary, ["--json", "publish"], undefined, fixture.env);
  expect(result.code).toBe(2);
  const json = JSON.parse(result.stdout);
  expect(json.ok).toBe(false);
  expect(json.message).toContain("Usage:");
});

test("preserves recovery for file-directory transitions", async () => {
  const fixture = await makeFixture();
  const source = join(fixture.root, "type-source");
  const upstream = join(fixture.root, "type-upstream.git");
  checked(run("git", ["init", "--bare", upstream], undefined, fixture.env), "init type remote");
  checked(run("git", ["init", "-b", "trunk", source], undefined, fixture.env), "init type source");
  await put(join(source, "skills/type/SKILL.md"), "name: type\nbase\n");
  await put(join(source, "skills/type/thing/child.txt"), "child\n");
  git(fixture, source, ["add", "."]);
  git(fixture, source, ["commit", "-m", "initial type package"]);
  git(fixture, source, ["remote", "add", "origin", upstream]);
  git(fixture, source, ["push", "-u", "origin", "trunk"]);
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "subscribe", upstream, "--skill", "type"]);

  await rm(join(source, "skills/type/thing"), { recursive: true, force: true });
  await put(join(source, "skills/type/thing"), "upstream file\n");
  git(fixture, source, ["add", "-A"]);
  git(fixture, source, ["commit", "-m", "replace directory with file"]);
  git(fixture, source, ["push"]);
  const updated = skillsync(fixture, ["--json", "update"]).json;
  expect(updated.results.some((item: any) => item.skill === "type" && item.status === "conflict")).toBe(true);
  expect((await stat(join(fixture.library, "type/thing"))).isDirectory()).toBe(true);
  expect(await readFile(join(fixture.library, "type/thing/child.txt"), "utf8")).toBe("child\n");
  expect(await readdir(join(fixture.config, "recovery"))).toHaveLength(1);
});

test("manages portable named sets over canonical library skills", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  skillsync(fixture, ["--json", "init"]);
  expect(skillsync(fixture, ["--json", "set", "create", "core"]).json.members).toEqual([]);
  expect(skillsync(fixture, ["--json", "set", "add", "core", "demo"]).json.status).toBe("added");
  expect(skillsync(fixture, ["--json", "set", "show", "core"]).json.members).toEqual(["demo"]);
  expect(skillsync(fixture, ["--json", "set", "remove", "core", "demo"]).json.status).toBe("removed");
  expect(skillsync(fixture, ["--json", "set", "show", "core"]).json.members).toEqual([]);
});

test("rejects symlinked library directories as set members", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "outside/SKILL.md"), "name: outside\n");
  skillsync(fixture, ["--json", "init"]);
  await symlink(join(fixture.library, "outside"), join(fixture.library, "link"), "dir");
  skillsync(fixture, ["--json", "set", "create", "core"]);
  const rejected = skillsync(fixture, ["--json", "set", "add", "core", "link"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("symlink");
});

test("rejects malformed persisted set members before listing or mutating", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "set", "create", "core"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.sets.core.members = ["library:../outside"];
  await writeFile(statePath, JSON.stringify(state));
  const listed = skillsync(fixture, ["--json", "set", "list"], false);
  expect(listed.json.ok).toBe(false);
  expect(listed.json.message).toContain("invalid set member");
  const changed = skillsync(fixture, ["--json", "set", "add", "core", "demo"], false);
  expect(changed.json.ok).toBe(false);
  expect(changed.json.message).toContain("invalid set member");
});

test("rejects state written by a newer Skillsync schema", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  state.version = 999;
  await writeFile(statePath, JSON.stringify(state));
  const rejected = skillsync(fixture, ["--json", "status"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("unsupported state version");
});

test("makes set membership retries safe and removes stale members", async () => {
  const fixture = await makeFixture();
  await put(join(fixture.library, "demo/SKILL.md"), "name: demo\n");
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "set", "create", "core"]);
  expect(skillsync(fixture, ["--json", "set", "add", "core", "demo"]).json.status).toBe("added");
  expect(skillsync(fixture, ["--json", "set", "add", "core", "demo"]).json.status).toBe("already_present");
  await rm(join(fixture.library, "demo"), { recursive: true, force: true });
  expect(skillsync(fixture, ["--json", "set", "remove", "core", "demo"]).json.status).toBe("removed");
  expect(skillsync(fixture, ["--json", "set", "remove", "core", "demo"]).json.status).toBe("already_absent");
});

test("rejects Windows-invalid portable set identifiers", async () => {
  const fixture = await makeFixture();
  skillsync(fixture, ["--json", "init"]);
  const reserved = skillsync(fixture, ["--json", "set", "create", "CON"], false);
  expect(reserved.json.ok).toBe(false);
  expect(reserved.json.message).toContain("invalid set name");
  const trailing = skillsync(fixture, ["--json", "set", "create", "name."], false);
  expect(trailing.json.ok).toBe(false);
  expect(trailing.json.message).toContain("invalid set name");
});

test("links one canonical skill into an explicit harness root and safely unlinks it", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  await put(join(fixture.library, "linked/SKILL.md"), "name: linked\\nbase\\n");
  const harness = join(fixture.root, "harness-skills");
  await mkdir(harness, { recursive: true });
  await put(join(harness, "bundled.txt"), "keep\\n");
  skillsync(fixture, ["--json", "init"]);
  const linked = skillsync(fixture, ["--json", "harness", "link", "--root", harness, "--skill", "linked"]).json;
  expect(linked.status).toBe("linked");
  expect((await lstat(join(harness, "linked"))).isSymbolicLink()).toBe(true);
  await put(join(harness, "linked/references/learning.md"), "canonical\\n");
  expect(await readFile(join(fixture.library, "linked/references/learning.md"), "utf8")).toBe("canonical\\n");
  expect(await readFile(join(harness, "bundled.txt"), "utf8")).toBe("keep\\n");
  const collision = skillsync(fixture, ["--json", "harness", "link", "--root", harness, "--skill", "linked"], false);
  expect(collision.json.ok).toBe(false);
  expect(collision.json.message).toContain("already exists");
  expect(Object.keys(skillsync(fixture, ["--json", "harness", "list"]).json.links)).toHaveLength(1);
  const unlinked = skillsync(fixture, ["--json", "harness", "unlink", "--root", harness, "--skill", "linked"]).json;
  expect(unlinked.canonical_retained).toBe(true);
  expect(await Bun.file(join(fixture.library, "linked/SKILL.md")).exists()).toBe(true);
  expect(await readFile(join(harness, "bundled.txt"), "utf8")).toBe("keep\\n");
  const invalid = skillsync(fixture, ["--json", "harness", "link", "--root", join(fixture.root, "missing"), "--skill", "linked"], false);
  expect(invalid.json.ok).toBe(false);
});

test("reports missing explicit harness links and rejects tampered link state", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  await put(join(fixture.library, "linked/SKILL.md"), "name: linked\n");
  const harness = join(fixture.root, "harness-skills");
  await mkdir(harness, { recursive: true });
  skillsync(fixture, ["--json", "init"]);
  skillsync(fixture, ["--json", "harness", "link", "--root", harness, "--skill", "linked"]);
  const healthy = skillsync(fixture, ["--json", "doctor"]).json;
  expect(healthy.harness_links[0].status).toBe("healthy");
  await unlink(join(harness, "linked"));
  const missing = skillsync(fixture, ["--json", "doctor"]).json;
  expect(missing.harness_links[0].status).toBe("missing");
  const statePath = join(fixture.config, "state.json");
  const state = JSON.parse(await readFile(statePath, "utf8"));
  const key = Object.keys(state.harness_links)[0];
  state.harness_links[key].link_path = join(fixture.root, "unrelated");
  await writeFile(statePath, JSON.stringify(state));
  const rejected = skillsync(fixture, ["--json", "status"], false);
  expect(rejected.json.ok).toBe(false);
  expect(rejected.json.message).toContain("harness link path does not match");
});

test("harness link collision is atomic and preserves the preexisting entry", async () => {
  if (process.platform === "win32") return;
  const fixture = await makeFixture();
  await put(join(fixture.library, "collision/SKILL.md"), "name: collision\\n");
  const harness = join(fixture.root, "collision-harness");
  await mkdir(harness, { recursive: true });
  await put(join(harness, "collision"), "unrelated\\n");
  skillsync(fixture, ["--json", "init"]);
  const rejected = skillsync(
    fixture,
    ["--json", "harness", "link", "--root", harness, "--skill", "collision"],
    false,
  );
  expect(rejected.json.ok).toBe(false);
  expect(await readFile(join(harness, "collision"), "utf8")).toBe("unrelated\\n");
  expect(Object.keys(skillsync(fixture, ["--json", "harness", "list"]).json.links)).toHaveLength(0);
});
